use std::sync::Arc;
use std::time::Duration;

use anyhow::Result as AnyhowResult;
use ethers::types::U256;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, instrument, warn};

use crate::contracts::{IdentityManager, SharedIdentityManager};
use crate::database::Database;
use crate::identity_tree::{Canonical, TreeVersion, TreeWithNextVersion};

const FINALIZE_ROOT_SLEEP_TIME: Duration = Duration::from_secs(5);

pub struct FinalizeRoots {
    database:             Arc<Database>,
    identity_manager:     SharedIdentityManager,
    finalized_tree:       TreeVersion<Canonical>,
    mined_roots_receiver: Arc<Mutex<mpsc::Receiver<U256>>>,
}

impl FinalizeRoots {
    pub fn new(
        database: Arc<Database>,
        identity_manager: SharedIdentityManager,
        finalized_tree: TreeVersion<Canonical>,
        mined_roots_receiver: Arc<Mutex<mpsc::Receiver<U256>>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            database,
            identity_manager,
            finalized_tree,
            mined_roots_receiver,
        })
    }

    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let mut mined_roots_receiver = self.mined_roots_receiver.lock().await;

        finalize_roots_loop(
            &self.database,
            &self.identity_manager,
            &self.finalized_tree,
            &mut mined_roots_receiver,
        )
        .await
    }
}

async fn finalize_roots_loop(
    database: &Database,
    identity_manager: &IdentityManager,
    finalized_tree: &TreeVersion<Canonical>,
    mined_roots_receiver: &mut mpsc::Receiver<U256>,
) -> AnyhowResult<()> {
    loop {
        let Some(mined_root) = mined_roots_receiver.recv().await else {
            warn!("Pending identities channel closed, terminating.");
            break;
        };

        finalize_root(mined_root, database, identity_manager, finalized_tree).await?;
    }

    Ok(())
}

#[instrument(level = "info", skip_all)]
async fn finalize_root(
    mined_root: U256,
    database: &Database,
    identity_manager: &IdentityManager,
    finalized_tree: &TreeVersion<Canonical>,
) -> AnyhowResult<()> {
    info!(?mined_root, "Finalizing root");

    loop {
        let is_root_finalized = identity_manager
            .is_root_mined_multi_chain(mined_root)
            .await?;

        if is_root_finalized {
            break;
        }

        tokio::time::sleep(FINALIZE_ROOT_SLEEP_TIME).await;
    }

    finalized_tree.apply_updates_up_to(mined_root.into());
    database.mark_root_as_mined(&mined_root.into()).await?;

    info!(?mined_root, "Root finalized");

    Ok(())
}