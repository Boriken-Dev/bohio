//! `bohio.cfg` as layers: the system file, then the user's, each merged over the one before.
//!
//! The library reads the files it is given and nothing else: which files, and in which order, is the
//! caller's choice (the command searches the system and user locations). Each layer is checked alone
//! for syntax, unknown keys and types, so an error names the file, line, column and key; the merged
//! value is then checked for ranges and the keys of the peers.
//!
//! Tables merge by key; arrays and scalars replace. An array is one setting (a root list, a program
//! list, `[[listen]]`), so a higher layer can narrow it; merging elements would only ever widen it.

use std::path::{Path, PathBuf};

use anycfg::{Callbacks, CodecError, Error, Formats, Loader, Merge, Options, Source};
use coqui_peer::PeerId;
use serde::Deserialize;
use toml::Value;

use super::{Config, ConfigError, position};
use crate::policy::Peers;

/// A configuration read from its layers.
#[derive(Debug, Clone, PartialEq)]
pub struct Layered {
    /// Every setting, merged.
    pub config: Config,
    /// The `[peer.<id>]` tables, merged: a key is accepted while any layer holds its table.
    pub peers: Peers,
    /// The files read, lowest layer first.
    pub files: Vec<PathBuf>,
}

/// Reading `bohio.cfg` layers.
#[derive(Debug, Clone, Copy)]
pub struct Layers;

/// The keys that govern layering and the access checks: the lowest layer alone sets them.
const LOWEST_ONLY: [&str; 2] = ["check_access", "user_layer"];

/// One layer as read.
struct Input {
    label: String,
    bytes: Vec<u8>,
}

impl Input {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

impl Layers {
    /// Reads `paths` as layers, the first lowest: every file must exist and be valid.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Read`] for a file that cannot be read, [`ConfigError::Parse`] for syntax, an unknown
    /// key or a wrong type in one layer (naming it, with the line, column and key), and
    /// [`ConfigError::Invalid`] for a value out of range, a table name that is not a peer id or a
    /// governing key (`[service] check_access`, `user_layer`) set above the lowest layer, naming the key
    /// and the highest layer that sets it.
    pub fn load(paths: &[PathBuf]) -> Result<Layered, ConfigError> {
        let inputs = paths
            .iter()
            .map(|path| read(path))
            .collect::<Result<Vec<_>, _>>()?;
        let mut layered = build(&inputs)?;
        layered.files = paths.to_vec();
        Ok(layered)
    }

    /// One layer of text, labelled `label` in errors.
    ///
    /// # Errors
    ///
    /// As [`Layers::load`].
    pub fn parse(text: &str, label: &str) -> Result<Layered, ConfigError> {
        build(&[Input {
            label: label.to_owned(),
            bytes: text.as_bytes().to_vec(),
        }])
    }

    /// The merged `[peer.<id>]` tables of `paths`, for a live check: a file that does not exist is
    /// skipped (a layer may be created later), other sections are not looked at, and an error is one of
    /// [`Layers::load`]'s about a layer's syntax or a peer.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Read`] for a file that exists and cannot be read, [`ConfigError::Parse`] and
    /// [`ConfigError::Invalid`] as [`Layers::load`], for the peers only.
    pub fn peers(paths: &[PathBuf]) -> Result<Peers, ConfigError> {
        let mut inputs = Vec::new();
        for path in paths.iter().filter(|path| path.is_file()) {
            inputs.push(read(path)?);
        }
        if inputs.is_empty() {
            return Ok(Peers::default());
        }
        let formats = formats();
        let values = values(&formats, &inputs)?;
        let only: PeersOnly = merged(&formats, &inputs, &values)?;
        check_ids(&only.peer, &inputs, &values)?;
        Ok(only.peer)
    }
}

/// A file's bytes, with its path as the label.
fn read(path: &Path) -> Result<Input, ConfigError> {
    let label = path.display().to_string();
    std::fs::read(path)
        .map(|bytes| Input {
            label: label.clone(),
            bytes,
        })
        .map_err(|error| ConfigError::Read {
            path: label,
            reason: error.kind().to_string(),
        })
}

/// Only the peers, whatever else the files hold.
#[derive(Deserialize, Default)]
struct PeersOnly {
    #[serde(default)]
    peer: Peers,
}

/// TOML over [`Value`], with `.cfg` fixed to it so a `#!` line is a comment.
fn formats() -> Formats<Value> {
    let mut formats = Formats::new();
    let codec = Callbacks::new(decode, |value: &Value| {
        toml::to_string(value)
            .map(String::into_bytes)
            .map_err(|_| CodecError::new().message("the value has no TOML form"))
    });
    // The names are fixed and valid: a registration cannot fail.
    let _ = formats.register("toml", ["toml"], codec);
    let _ = formats.register_extension("cfg", "toml");
    formats
}

/// The parser's own text and position, which hold none of the document.
fn decode(bytes: &[u8]) -> Result<Value, CodecError> {
    let text = core::str::from_utf8(bytes).map_err(|error| {
        CodecError::at_offset(bytes, error.valid_up_to()).message("not valid UTF-8")
    })?;
    toml::from_str::<toml::Table>(text)
        .map(Value::Table)
        .map_err(|error| {
            let failure = error.span().map_or_else(CodecError::new, |span| {
                CodecError::at_offset(bytes, span.start)
            });
            failure.message(error.message())
        })
}

/// Tables merge by key, anything else is replaced.
fn merge(previous: &mut Value, next: Value, strategy: Merge, depth: usize) -> Result<(), Error> {
    fn deep(previous: &mut Value, next: Value, left: usize) -> Result<(), Error> {
        match (previous, next) {
            (Value::Table(into), Value::Table(from)) => {
                if left == 0 {
                    return Err(Error::TooDeep { limit: 0 });
                }
                for (key, value) in from {
                    match into.get_mut(&key) {
                        Some(existing) => deep(existing, value, left - 1)?,
                        None => {
                            into.insert(key, value);
                        }
                    }
                }
                Ok(())
            }
            (previous, next) => {
                *previous = next;
                Ok(())
            }
        }
    }
    match strategy {
        Merge::Replace => {
            *previous = next;
            Ok(())
        }
        _ => deep(previous, next, depth),
    }
}

/// Each layer's own value, its syntax checked; the second layer on may not set a governing key.
fn values(formats: &Formats<Value>, inputs: &[Input]) -> Result<Vec<Value>, ConfigError> {
    let mut values = Vec::new();
    for (index, input) in inputs.iter().enumerate() {
        let value = formats
            .from_slice(&input.bytes, Options::new().format("toml"))
            .map_err(|error| layer_error(input, &error))?;
        if index > 0 {
            for key in LOWEST_ONLY {
                if has_path(&value, &["service", key]) {
                    return Err(ConfigError::Invalid {
                        path: input.label.clone(),
                        key: format!("service.{key}"),
                        message: "is read from the lowest layer only: remove it here".to_owned(),
                    });
                }
            }
        }
        values.push(value);
    }
    Ok(values)
}

/// The layers merged and read as `T`.
fn merged<T: serde::de::DeserializeOwned>(
    formats: &Formats<Value>,
    inputs: &[Input],
    values: &[Value],
) -> Result<T, ConfigError> {
    let loader = Loader::new(formats, "toml").merge(Merge::Deep);
    let sources = inputs
        .iter()
        .map(|input| Source::bytes(&input.bytes).format("toml"));
    loader
        .load_typed(sources, &merge)
        .map_err(|error| match error {
            Error::Source { index, cause } => inputs
                .get(index)
                .map_or_else(|| other(&cause), |input| layer_error(input, &cause)),
            other_error => match other_error {
                Error::Conversion { path, message } => {
                    // The layers are each valid alone: the merged value differs in what a higher
                    // layer set over a lower one, so the highest layer that sets the key is named.
                    let segments: Vec<&str> = path
                        .as_deref()
                        .map(|path| {
                            path.split(['.', '['])
                                .map(|part| part.trim_end_matches(']'))
                                .collect()
                        })
                        .unwrap_or_default();
                    let at = setter(inputs, values, &segments);
                    let input = &inputs[at];
                    conversion(input, path, message)
                }
                other_error => other(&other_error),
            },
        })
}

fn other(error: &Error) -> ConfigError {
    ConfigError::Parse {
        path: String::new(),
        line: 1,
        column: 1,
        key: None,
        message: error.to_string(),
    }
}

/// `error` about one layer, with where in its text it is.
fn layer_error(input: &Input, error: &Error) -> ConfigError {
    match error {
        Error::Source { cause, .. } | Error::File { cause, .. } => layer_error(input, cause),
        Error::Parse {
            line,
            column,
            message,
            ..
        } => ConfigError::Parse {
            path: input.label.clone(),
            line: line.unwrap_or(1),
            column: column.unwrap_or(1),
            key: None,
            message: message
                .clone()
                .unwrap_or_else(|| "not valid TOML".to_owned()),
        },
        Error::Conversion { path, message } => conversion(input, path.clone(), message.clone()),
        other_error => ConfigError::Parse {
            path: input.label.clone(),
            line: 1,
            column: 1,
            key: None,
            message: other_error.to_string(),
        },
    }
}

fn conversion(input: &Input, path: Option<String>, message: Option<String>) -> ConfigError {
    let (line, column) = path
        .as_deref()
        .map_or((1, 1), |path| locate(&input.text(), path));
    ConfigError::Parse {
        path: input.label.clone(),
        line,
        column,
        key: path,
        message: message.unwrap_or_else(|| "is not valid".to_owned()),
    }
}

/// Where the walk of a path through a parsed document stands.
enum Cursor<'a> {
    Item(&'a toml_edit::Item),
    Table(&'a toml_edit::Table),
    Value(&'a toml_edit::Value),
}

type Step<'a> = Option<(Option<core::ops::Range<usize>>, Cursor<'a>)>;

fn into_key<'a>(cursor: &Cursor<'a>, name: &str) -> Step<'a> {
    use toml_edit::Item;
    let inline = |value: &'a toml_edit::Value| {
        value
            .as_inline_table()
            .and_then(|table| table.get_key_value(name))
            .map(|(key, value)| (key.span(), Cursor::Item(value)))
    };
    match *cursor {
        Cursor::Item(Item::Table(table)) | Cursor::Table(table) => table
            .get_key_value(name)
            .map(|(key, item)| (key.span(), Cursor::Item(item))),
        Cursor::Item(Item::Value(value)) | Cursor::Value(value) => inline(value),
        Cursor::Item(_) => None,
    }
}

fn into_index<'a>(cursor: &Cursor<'a>, index: usize) -> Step<'a> {
    use toml_edit::Item;
    let element = |value: &'a toml_edit::Value| {
        value
            .as_array()
            .and_then(|array| array.get(index))
            .map(|value| (value.span(), Cursor::Value(value)))
    };
    match *cursor {
        Cursor::Item(Item::ArrayOfTables(tables)) => tables
            .get(index)
            .map(|table| (table.span(), Cursor::Table(table))),
        Cursor::Item(Item::Value(value)) | Cursor::Value(value) => element(value),
        Cursor::Item(_) | Cursor::Table(_) => None,
    }
}

/// The line and column where `path` (`link.max_streams`, `listen[1].endpoint`) is in `text`: its key,
/// or the nearest table or entry that holds it.
fn locate(text: &str, path: &str) -> (usize, usize) {
    let Ok(document) = toml_edit::Document::parse(text) else {
        return (1, 1);
    };
    let mut offset = 0_usize;
    let mut cursor = Cursor::Item(document.as_item());
    'walk: for part in path.split('.') {
        let (name, indexes) = part.find('[').map_or((part, ""), |at| part.split_at(at));
        if !name.is_empty() {
            let Some((span, next)) = into_key(&cursor, name) else {
                break 'walk;
            };
            offset = span.map_or(offset, |span| span.start);
            cursor = next;
        }
        for digits in indexes
            .split(['[', ']'])
            .filter(|digits| !digits.is_empty())
        {
            let Some((span, next)) = digits
                .parse::<usize>()
                .ok()
                .and_then(|index| into_index(&cursor, index))
            else {
                break 'walk;
            };
            offset = span.map_or(offset, |span| span.start);
            cursor = next;
        }
    }
    position(text, offset)
}

/// Whether `value` holds `segments` (an array on the way counts as holding the rest).
fn has_path(value: &Value, segments: &[&str]) -> bool {
    match (value, segments.split_first()) {
        (_, None) | (Value::Array(_), Some(_)) => true,
        (Value::Table(table), Some((first, rest))) => {
            table.get(*first).is_some_and(|inner| has_path(inner, rest))
        }
        _ => false,
    }
}

/// The index of the highest layer that sets `segments`, or the lowest when none does.
fn setter(inputs: &[Input], values: &[Value], segments: &[&str]) -> usize {
    (0..inputs.len())
        .rev()
        .find(|index| has_path(&values[*index], segments))
        .unwrap_or(0)
}

/// Every table name under `[peer]` is a peer id.
fn check_ids(peers: &Peers, inputs: &[Input], values: &[Value]) -> Result<(), ConfigError> {
    for name in peers.ids() {
        if name.parse::<PeerId>().is_err() {
            let at = setter(inputs, values, &["peer", name]);
            return Err(ConfigError::Invalid {
                path: inputs[at].label.clone(),
                key: format!("peer.{name}"),
                message: "is not a peer id: name each peer table by the id `bohio id` prints"
                    .to_owned(),
            });
        }
    }
    Ok(())
}

/// The layers, checked each alone and then merged.
fn build(inputs: &[Input]) -> Result<Layered, ConfigError> {
    if inputs.is_empty() {
        return Ok(Layered {
            config: Config::default(),
            peers: Peers::default(),
            files: Vec::new(),
        });
    }
    let formats = formats();
    for input in inputs {
        formats
            .from_slice_typed::<Config>(&input.bytes, Options::new().format("toml"))
            .map_err(|error| layer_error(input, &error))?;
    }
    let values = values(&formats, inputs)?;
    let config: Config = merged(&formats, inputs, &values)?;
    if let Err(ConfigError::Invalid { key, message, .. }) = config.validate("") {
        let segments: Vec<&str> = key.split('.').collect();
        let at = setter(inputs, &values, &segments);
        return Err(ConfigError::Invalid {
            path: inputs[at].label.clone(),
            key,
            message,
        });
    }
    check_ids(&config.peer, inputs, &values)?;
    Ok(Layered {
        peers: config.peer.clone(),
        config,
        files: Vec::new(),
    })
}
