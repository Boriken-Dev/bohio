// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A provider library that serves processes and no console, to prove a node maps a library of any Bohío kind.

use bohio_api::guatiao::Provider;
use bohio_api::guatiao::library::{Object, ProviderError};
use bohio_api::model::{ExecStart, SourceInfo};
use bohio_api::{ExecSource, Process, unsupported};

/// A source that starts nothing: loading it is the point.
#[derive(Default, Provider)]
#[provider(
    ExecSource,
    id = "library-exec",
    name = "exec only library",
    crate = ::bohio_api::guatiao
)]
pub struct Idle;

impl ExecSource for Idle {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        Ok(SourceInfo::new("library-exec", &[], &[]))
    }

    fn start(&self, _request: ExecStart) -> Result<Object<dyn Process>, ProviderError> {
        Err(unsupported("this source starts no process"))
    }
}

bohio_api::guatiao::providers!(Idle);
