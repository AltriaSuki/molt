//! The networks the commands and processes of each workspace share (#225):
//! see [`LocalNet`]. A workspace has one while something runs in it, and
//! the first command after nothing does gets a new one. A workspace whose
//! directory was replaced since (a dropped fork made again at the same
//! path) gets a new one too, so nothing the old copy left running is
//! reached from the new.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use molt_proto::RemoteError;

use crate::error::{failed, invalid};
use crate::resources::{self, Setup};
use crate::sandbox::{LocalNet, Unavailable};
use crate::shell::lock;
use crate::SandboxPolicy;

/// Which directory a workspace is: its device and inode. A directory made
/// again at the same path is another.
pub(crate) type Identity = (u64, u64);

/// The identity of the directory `ws` names now; none when it is gone.
pub(crate) fn identity(ws: &Path) -> Option<Identity> {
    std::fs::metadata(ws).ok().filter(|meta| meta.is_dir()).map(|meta| (meta.dev(), meta.ino()))
}

/// Each workspace's network, while a command or process holds it.
#[derive(Default)]
pub(crate) struct Networks {
    table: Mutex<HashMap<PathBuf, (Identity, Weak<LocalNet>)>>,
}

impl Networks {
    /// The network a command in the canonical workspace `ws` joins: the one
    /// what runs there now is on, or a new one. None when `policy` gives
    /// each command a network of its own, or the host's.
    pub(crate) fn join(&self, policy: &SandboxPolicy, ws: &Path) -> Result<Option<Arc<LocalNet>>, RemoteError> {
        if !policy.shares_local_network() {
            return Ok(None);
        }
        let Some(id) = identity(ws) else {
            return Err(invalid(format!("workspace {} does not exist", ws.display())));
        };
        // Made under the lock, so two commands starting at once share one.
        let mut table = lock(&self.table);
        table.retain(|_, (_, net)| net.strong_count() > 0);
        if let Some(net) = table.get(ws).filter(|(was, _)| *was == id).and_then(|(_, net)| net.upgrade()) {
            return Ok(Some(net));
        }
        let net = Arc::new(LocalNet::start(policy).map_err(|e| refused(e, policy))?);
        table.insert(ws.to_owned(), (id, Arc::downgrade(&net)));
        Ok(Some(net))
    }

    /// How many networks something still runs in.
    pub(crate) fn live(&self) -> usize {
        lock(&self.table).values().filter(|(_, net)| net.strong_count() > 0).count()
    }
}

/// The error for a network that could not be made: the command did not run.
fn refused(e: anyhow::Error, policy: &SandboxPolicy) -> RemoteError {
    match e.downcast_ref::<Unavailable>() {
        Some(Unavailable { setup: Setup::Short(resource), .. }) => {
            resources::short_of(*resource, &e.to_string(), 1, Some(policy))
        }
        Some(Unavailable { setup, .. }) => resources::no_isolation(*setup, &e.to_string()),
        None => failed(format!("the workspace's network could not be made: {e:#}")),
    }
}
