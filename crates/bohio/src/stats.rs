//! What the host is running on, for the control call `stats`.

#![forbid(unsafe_code)]

use bohio_proto::control::{Disk, Memory, Stats, Swap};
use sysinfo::{Disks, System};

/// The host's name, uptime, processors, load, memory and disks as the operating system reports them now.
///
/// A value the system does not report is empty or null: the load averages on Windows, a filesystem name a mount does not
/// carry. The processors are the logical ones this process may run on.
pub fn collect() -> Stats {
    let mut system = System::new();
    system.refresh_memory();
    let disks = Disks::new_with_refreshed_list();
    Stats {
        hostname: System::host_name(),
        uptime_s: System::uptime(),
        cpus: std::thread::available_parallelism().map_or(1, |count| count.get() as u64),
        load: cfg!(unix).then(|| {
            let load = System::load_average();
            [load.one, load.five, load.fifteen]
        }),
        memory: Memory {
            total: system.total_memory(),
            available: system.available_memory(),
        },
        swap: Swap {
            total: system.total_swap(),
            free: system.free_swap(),
        },
        disks: disks
            .list()
            .iter()
            .map(|disk| Disk {
                mount: disk.mount_point().to_string_lossy().into_owned(),
                filesystem: disk.file_system().to_string_lossy().into_owned(),
                total: disk.total_space(),
                available: disk.available_space(),
            })
            .collect(),
    }
}
