//! `ashd-fm --dbus-service`: owns `org.freedesktop.FileManager1` on the session bus so that
//! "Show in folder" / "Open containing folder" in browsers, chat apps etc. opens ashd-fm with
//! the item under the cursor. D-Bus-activated through the service file in packaging/.

use std::{path::PathBuf, process::Command};

use zbus::{blocking::connection, interface};

use crate::fs_model::path_from_arg;

struct FileManager1;

fn launch(args: &[&str], path: &PathBuf) {
    let Ok(exe) = std::env::current_exe() else { return };
    // Reap the child in a thread so finished windows do not linger as zombies.
    if let Ok(mut child) = Command::new(exe).args(args).arg(path).spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

#[interface(name = "org.freedesktop.FileManager1")]
impl FileManager1 {
    fn show_items(&self, uris: Vec<String>, _startup_id: &str) {
        // One window per distinct parent folder, cursor on the (first) item in it.
        let mut seen = Vec::new();
        for path in uris.iter().map(|u| path_from_arg(u)) {
            let parent = path.parent().map(PathBuf::from);
            if !seen.contains(&parent) {
                seen.push(parent);
                launch(&["--select"], &path);
            }
        }
    }

    fn show_folders(&self, uris: Vec<String>, _startup_id: &str) {
        for path in uris.iter().map(|u| path_from_arg(u)) {
            launch(&[], &path);
        }
    }

    fn show_item_properties(&self, uris: Vec<String>, startup_id: &str) {
        self.show_items(uris, startup_id);
    }
}

pub fn run() -> zbus::Result<()> {
    let _connection = connection::Builder::session()?
        .name("org.freedesktop.FileManager1")?
        .serve_at("/org/freedesktop/FileManager1", FileManager1)?
        .build()?;
    loop {
        std::thread::park();
    }
}
