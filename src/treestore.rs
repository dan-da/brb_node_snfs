
use brb_dt_tree::BRBTree;
use sn_fs::{FsTreeStore, FsTreeIdType, FsTreeMetaType, ActorType};

/// An FsTreeStore built on crdt_tree::TreeStore
pub struct FsBrbTreeReplicaStore(BRBTree<FsTreeIdType, FsTreeMetaType, ActorType>);

impl FsBrbTreeReplicaStore {
    /// instantiate new FsBrbTreeReplicaStore
    pub fn new(actor: ActorType) -> Self {
        Self(BRBTree::new(actor))
    }
}

impl FsTreeStore for FsBrbTreeReplicaStore {
    fn opmove(&self, parent_id: TreeIdType, meta: TreeMetaType, child_id: TreeIdType) -> FsOpMove {
        self.0.opmove(parent_id, meta, child_id)
    }

    fn apply_op(&mut self, op: FsOpMove) {
        self.0.apply(op)
    }

    fn state(&self) -> &FsState {
        self.0.treestate()
    }

    fn time(&self) -> &FsClock {
        self.0.treereplica().time()
    }

    fn truncate_log(&mut self) -> bool {
        false
        // we need to impl treereplica_mut() first.
        // self.0.treereplica_mut().truncate_log()
    }
}
