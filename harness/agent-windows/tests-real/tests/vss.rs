//! Real Volume Shadow Copy conformance (WP-ADM-05.T3, `vss` in `matrix.toml`).
//!
//! Takes a real shadow copy of the system volume through the `#[cfg(windows)]` `vssadmin`
//! client, lists it, exposes it read-only and deletes it. The shadow is this process's own
//! and is deleted on every path; the suite never deletes a shadow it did not create.

use std::path::Path;

use spot_agent_windows_sys::windows::WindowsVss;
use spot_agent_windows_sys::{VssApi, VolumeSet, WriterSet};
use spot_agent_windows_real::Cleanup;

#[test]
fn throwaway_shadow_lifecycle() {
    let vss = WindowsVss::new();

    // The real client tracks only the shadows this process created, so a fresh instance
    // starts empty: nothing the machine already has is ever touched.
    assert!(
        vss.list().expect("listing must succeed").is_empty(),
        "the harness must start with no shadow copies of its own"
    );

    let volume = vss
        .volume_of(Path::new(r"C:\"))
        .expect("the system volume must resolve");
    let volumes = VolumeSet::one(volume);

    // The creation instant is an argument; the harness passes 0, so no clock is read.
    let shadow = vss
        .create(&volumes, &WriterSet::all(), 0)
        .expect("creating a shadow copy must succeed");

    let mut cleanup = Cleanup::new();
    {
        let doomed = shadow.id.clone();
        cleanup.push(move || {
            // A fresh client can delete by id: the real delete asks `vssadmin` by id and
            // does not require the shadow to be in this instance's own list.
            let _ = WindowsVss::new().delete(&doomed);
        });
    }

    let listed = vss.list().expect("listing must succeed");
    assert!(listed.iter().any(|copy| copy.id == shadow.id));

    // Expose read-only: the returned device path is this process's own view of the
    // shadow, not a symlink or a mount that would outlive the test.
    let exposed = vss
        .expose_read_only(&shadow.id, Path::new(r"C:\Windows\System32\notepad.exe"))
        .expect("exposing the shadow read-only must succeed");
    assert!(!exposed.as_os_str().is_empty());

    vss.delete(&shadow.id)
        .expect("deleting the shadow must succeed");
    assert!(
        !vss
            .list()
            .expect("listing must succeed")
            .iter()
            .any(|copy| copy.id == shadow.id),
        "the shadow must be gone after delete"
    );
}
