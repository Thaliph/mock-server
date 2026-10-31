//! Process hardening (spec §5, TLS-16): no core dump and no memory or `/proc` access from other
//! processes of the pod, applied before any key is generated.

use std::io;

use nix::sys::prctl::set_dumpable;
use nix::sys::resource::{Resource, setrlimit};

/// Sets `RLIMIT_CORE` to 0 and the process non-dumpable (`PR_SET_DUMPABLE` 0).
pub fn harden() -> io::Result<()> {
    setrlimit(Resource::RLIMIT_CORE, 0, 0).map_err(io::Error::from)?;
    set_dumpable(false).map_err(io::Error::from)
}
