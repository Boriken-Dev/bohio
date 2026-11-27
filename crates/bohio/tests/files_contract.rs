// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `native` files source passes the files kind's contract suite and reports what it does.

use bohio::access::Sources;
use bohio_api::contract::{ContractSettings, files};

#[test]
fn the_native_files_source_passes_the_contract_suite() {
    let sources = Sources::default();
    let source = sources.files(None).expect("the node has a files source");
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path().to_str().expect("a text path").to_owned();
    files(source.get(), &root, &ContractSettings::default());
}

#[test]
fn the_native_files_source_says_it_is_confined_and_where_it_stops() {
    let sources = Sources::default();
    let source = sources.files(None).expect("the node has a files source");
    let info = source.get().describe().expect("it describes itself");
    assert!(info.available);
    assert!(info.offers.iter().any(|word| word == "confined"));
    assert!(info.offers.iter().any(|word| word == "atomic_replace"));
    assert!(info.lacks("statvfs"), "no statvfs is claimed");
}

/// The native source with one word's behaviour changed against what `describe` says.
#[cfg(unix)]
mod liar {
    use bohio_api::files::{failure, word};
    use bohio_api::guatiao::library::{Object, ProviderError};
    use bohio_api::model::{FileStat, OpenModel, SourceInfo, TimeModel, VfsStat};
    use bohio_api::{DirReader, Directory, File, FilesSource, vocab};

    use super::{ContractSettings, Sources, files};

    #[derive(Clone, Copy)]
    enum Lie {
        /// Offers `symlink` and refuses it.
        OffersAndFails,
        /// Lacks `symlink` and takes it.
        LacksAndTakes,
    }

    struct Dir(Object<dyn Directory>, Lie);

    impl Directory for Dir {
        fn open_dir(&self, relative: &str) -> Result<Object<dyn Directory>, ProviderError> {
            Ok(Dir(self.0.open_dir(relative)?, self.1).into_object())
        }
        fn stat(&self, relative: &str, follow: bool) -> Result<FileStat, ProviderError> {
            self.0.stat(relative, follow)
        }
        fn read_dir(&self, relative: &str) -> Result<Object<dyn DirReader>, ProviderError> {
            self.0.read_dir(relative)
        }
        fn open_file(
            &self,
            relative: &str,
            options: OpenModel,
        ) -> Result<Object<dyn File>, ProviderError> {
            self.0.open_file(relative, options)
        }
        fn create_dir(
            &self,
            relative: &str,
            mode: Option<u32>,
            parents: bool,
        ) -> Result<(), ProviderError> {
            self.0.create_dir(relative, mode, parents)
        }
        fn remove_dir(&self, relative: &str) -> Result<(), ProviderError> {
            self.0.remove_dir(relative)
        }
        fn remove_dir_all(&self, relative: &str) -> Result<(), ProviderError> {
            self.0.remove_dir_all(relative)
        }
        fn remove_file(&self, relative: &str) -> Result<(), ProviderError> {
            self.0.remove_file(relative)
        }
        fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<(), ProviderError> {
            self.0.rename(from, to, overwrite)
        }
        fn hard_link(&self, from: &str, to: &str) -> Result<(), ProviderError> {
            self.0.hard_link(from, to)
        }
        fn symlink(
            &self,
            target: &str,
            link: &str,
            to_directory: bool,
        ) -> Result<(), ProviderError> {
            match self.1 {
                Lie::OffersAndFails => Err(failure(word::IO, "the link could not be made")),
                Lie::LacksAndTakes => self.0.symlink(target, link, to_directory),
            }
        }
        fn read_link(&self, relative: &str) -> Result<String, ProviderError> {
            self.0.read_link(relative)
        }
        fn set_times(
            &self,
            relative: &str,
            accessed: Option<TimeModel>,
            modified: Option<TimeModel>,
        ) -> Result<(), ProviderError> {
            self.0.set_times(relative, accessed, modified)
        }
        fn set_mode(&self, relative: &str, mode: u32) -> Result<(), ProviderError> {
            self.0.set_mode(relative, mode)
        }
        fn set_readonly(&self, relative: &str, readonly: bool) -> Result<(), ProviderError> {
            self.0.set_readonly(relative, readonly)
        }
        fn statvfs(&self, relative: &str) -> Result<VfsStat, ProviderError> {
            self.0.statvfs(relative)
        }
    }

    struct Source<'a>(&'a dyn FilesSource, Lie);

    impl FilesSource for Source<'_> {
        fn describe(&self) -> Result<SourceInfo, ProviderError> {
            let mut info = self.0.describe()?;
            if matches!(self.1, Lie::LacksAndTakes) {
                info.offers.retain(|word| word != vocab::files::SYMLINK);
                info.lacks.push(vocab::files::SYMLINK.to_owned());
            }
            Ok(info)
        }
        fn open_dir(&self, path: &str) -> Result<Object<dyn Directory>, ProviderError> {
            Ok(Dir(self.0.open_dir(path)?, self.1).into_object())
        }
    }

    fn run(lie: Lie) {
        let sources = Sources::default();
        let source = sources.files(None).expect("the node has a files source");
        let dir = tempfile::tempdir().expect("a scratch directory");
        let root = dir.path().to_str().expect("a text path").to_owned();
        files(
            &Source(source.get(), lie),
            &root,
            &ContractSettings::default(),
        );
    }

    #[test]
    #[should_panic(expected = "offers `symlink` and fails it")]
    fn a_source_that_offers_symlink_and_fails_it_fails_the_suite() {
        run(Lie::OffersAndFails);
    }

    #[test]
    #[should_panic(expected = "a symlink succeeded and must fail")]
    fn a_source_that_lacks_symlink_and_takes_it_fails_the_suite() {
        run(Lie::LacksAndTakes);
    }
}
