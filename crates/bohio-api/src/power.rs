// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The power kind: the power of the system a node is for, through a BMC, a hypervisor or a smart switch.

use guatiao::library::ProviderError;

use crate::model::{PowerState, SourceInfo};

/// A source of power control.
#[guatiao::kind(name = "bohio_power")]
pub trait PowerSource: Send + Sync {
    /// What this source is and can do now; `offers` lists the actions it takes ([`crate::vocab::power`]).
    ///
    /// # Errors
    ///
    /// Only a source that cannot even describe itself.
    fn describe(&self) -> Result<SourceInfo, ProviderError>;

    /// The system's power state now.
    ///
    /// # Errors
    ///
    /// `GONE` when the source cannot be reached.
    fn state(&self) -> Result<PowerState, ProviderError>;

    /// Takes an action (`on`, `off`, `soft_off`, `cycle`, `reset`, `nmi`) and answers the state it left the system
    /// in. A call may block until the action finished; the host bounds it by `[power] action_timeout`.
    ///
    /// # Errors
    ///
    /// `UNSUPPORTED` for an action the source does not take, `BAD_VALUE` for a word it does not know, `GONE`.
    fn act(&self, action: &str) -> Result<PowerState, ProviderError>;
}
