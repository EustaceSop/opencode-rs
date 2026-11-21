use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::{PermissionAsker, PermissionRequest, ToolContext, ToolError};

#[derive(Default)]
pub struct TestAsker {
    pub requests: Mutex<Vec<PermissionRequest>>,
    pub deny: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl PermissionAsker for TestAsker {
    async fn ask(&self, req: PermissionRequest) -> Result<(), ToolError> {
        let denied = self.deny.lock().unwrap().contains(&req.permission);
        self.requests.lock().unwrap().push(req);
        if denied {
            Err(ToolError::Rejected)
        } else {
            Ok(())
        }
    }
}

pub struct Fixture {
    _dir: tempfile::TempDir,
    pub data: tempfile::TempDir,
    pub root: PathBuf,
    pub asker: Arc<TestAsker>,
    pub abort: CancellationToken,
    pub hooks: Option<Arc<dyn crate::WorkspaceHooks>>,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        Self {
            _dir: dir,
            data: tempfile::tempdir().unwrap(),
            root,
            asker: Arc::new(TestAsker::default()),
            abort: CancellationToken::new(),
            hooks: None,
        }
    }

    pub fn ctx(&self) -> ToolContext {
        ToolContext {
            session_id: "ses_test".into(),
            message_id: "msg_test".into(),
            call_id: "call_test".into(),
            agent: "build".into(),
            cwd: self.root.clone(),
            worktree: self.root.clone(),
            data_dir: self.data.path().to_path_buf(),
            abort: self.abort.clone(),
            permissions: self.asker.clone(),
            hooks: self.hooks.clone(),
        }
    }

    pub fn write(&self, rel: &str, content: &str) -> PathBuf {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.root.join(rel)).unwrap()
    }

    pub fn requests(&self) -> Vec<PermissionRequest> {
        self.asker.requests.lock().unwrap().clone()
    }

    pub fn deny(&self, permission: &str) {
        self.asker.deny.lock().unwrap().push(permission.to_string());
    }
}
