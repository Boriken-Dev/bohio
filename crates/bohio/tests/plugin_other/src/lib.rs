// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A library of a kind that is not one of Bohío's, to prove a node maps only the libraries that declare its kinds.

use guatiao::Provider;
use guatiao::library::ProviderError;

/// A kind of its own.
#[guatiao::kind(name = "other_greeter")]
pub trait Greeter: Send + Sync {
    /// Says hello.
    fn greet(&self) -> Result<String, ProviderError>;
}

/// The only provider.
#[derive(Default, Provider)]
#[provider(Greeter)]
pub struct Hello;

impl Greeter for Hello {
    fn greet(&self) -> Result<String, ProviderError> {
        Ok("hello".to_owned())
    }
}

guatiao::providers!(Hello);
