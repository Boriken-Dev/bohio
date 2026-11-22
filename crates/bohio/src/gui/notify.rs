//! The desktop notification of a presence notice, through the notification service of the platform.

use std::sync::atomic::{AtomicBool, Ordering};

use super::presence::{Decide, Notice, Notifier};

/// Shows notices as desktop notifications. A missing notification service is logged once and the notice dropped: the
/// indicator informs, it does not gate. On Linux, when the service advertises `actions`, a notice that asks carries Allow
/// and Deny; elsewhere the decision is made with `bohio gui allow` and `deny`.
#[derive(Debug, Default)]
pub struct DesktopNotifier {
    warned: AtomicBool,
}

impl DesktopNotifier {
    /// A notifier that has not yet failed.
    pub fn new() -> Self {
        Self::default()
    }

    fn failed(&self, reason: &dyn core::fmt::Display) {
        if !self.warned.swap(true, Ordering::SeqCst) {
            tracing::warn!(%reason, "the desktop notification could not be shown");
        }
    }
}

impl Notifier for DesktopNotifier {
    fn notify(&self, notice: &Notice, decide: Option<Decide>) {
        let mut shown = notify_rust::Notification::new();
        shown
            .appname("Bohío")
            .summary(&notice.title)
            .body(&notice.body);
        #[cfg(all(unix, not(target_os = "macos")))]
        let decide = {
            let actions = notify_rust::get_capabilities()
                .is_ok_and(|capabilities| capabilities.iter().any(|name| name == "actions"));
            match decide {
                Some(decide) if actions => {
                    shown.action("allow", "Allow").action("deny", "Deny");
                    Some(decide)
                }
                _ => None,
            }
        };
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        drop(decide);
        match shown.show() {
            #[cfg(all(unix, not(target_os = "macos")))]
            Ok(handle) => {
                if let Some(decide) = decide {
                    let _ = std::thread::Builder::new()
                        .name("bohio-gui-notice".to_owned())
                        .spawn(move || {
                            handle.wait_for_action(|action| match action {
                                "allow" => decide(true),
                                "deny" => decide(false),
                                _ => {}
                            });
                        });
                }
            }
            #[cfg(not(all(unix, not(target_os = "macos"))))]
            Ok(_) => {}
            Err(error) => self.failed(&error),
        }
    }
}
