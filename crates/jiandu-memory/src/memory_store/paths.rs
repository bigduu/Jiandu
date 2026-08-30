use std::path::{Path, PathBuf};

use crate::ProjectId;

use super::types::MemoryScope;

pub const MEMORY_ROOT_DIR: &str = "memory";
pub const MEMORY_VERSION_DIR: &str = "v1";
pub const SESSIONS_DIR: &str = "sessions";
pub const SCOPES_DIR: &str = "scopes";
pub const GLOBAL_DIR: &str = "global";
pub const PROJECTS_DIR: &str = "projects";
pub const NOTE_DIR: &str = "note";
pub const STATE_DIR: &str = "state";
pub const INDEXES_DIR: &str = "indexes";
pub const VIEWS_DIR: &str = "views";
pub const LOGS_DIR: &str = "logs";
pub const TOPICS_DIR: &str = "topics";

#[derive(Debug, Clone)]
pub struct MemoryPathResolver {
    data_dir: PathBuf,
    root: PathBuf,
    project_id: Option<ProjectId>,
}

/// Explicit first-class Project memory layout.
///
/// Project identity is supplied by the caller and is never derived from a
/// workspace path.
#[derive(Debug, Clone)]
pub struct ProjectMemoryPathResolver {
    data_dir: PathBuf,
}

impl ProjectMemoryPathResolver {
    pub fn from_data_dir(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    pub fn project_home(&self, project_id: &ProjectId) -> PathBuf {
        self.data_dir.join(PROJECTS_DIR).join(project_id.as_str())
    }

    pub fn memory_root(&self, project_id: &ProjectId) -> PathBuf {
        self.project_home(project_id)
            .join(MEMORY_ROOT_DIR)
            .join(MEMORY_VERSION_DIR)
    }
}

impl MemoryPathResolver {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let data_dir = infer_data_dir_from_root(&root);
        Self {
            data_dir,
            root,
            project_id: None,
        }
    }

    pub fn from_data_dir(data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        let root = data_dir.join(MEMORY_ROOT_DIR).join(MEMORY_VERSION_DIR);
        Self {
            data_dir,
            root,
            project_id: None,
        }
    }

    /// Bind Project-scope paths to one validated first-class Project id.
    ///
    /// This is explicit rather than inferred from a workspace path.
    pub fn for_project(&self, project_id: &ProjectId) -> Self {
        Self {
            data_dir: self.data_dir.clone(),
            root: self.root.clone(),
            project_id: Some(project_id.clone()),
        }
    }

    pub fn project_id(&self) -> Option<&ProjectId> {
        self.project_id.as_ref()
    }

    pub fn project_read_roots(&self, project_key: &str) -> Vec<PathBuf> {
        vec![self.project_root(project_key)]
    }

    pub fn scope_read_roots(&self, scope: MemoryScope, project_key: Option<&str>) -> Vec<PathBuf> {
        match scope {
            MemoryScope::Project => self.project_read_roots(project_key.unwrap_or("unknown")),
            _ => vec![self.scope_root(scope, project_key)],
        }
    }

    pub fn data_dir(&self) -> PathBuf {
        self.data_dir.clone()
    }

    pub fn root(&self) -> PathBuf {
        self.root.clone()
    }

    pub fn sessions_root(&self) -> PathBuf {
        self.root.join(SESSIONS_DIR)
    }

    pub fn session_root(&self, session_id: &str) -> PathBuf {
        self.sessions_root().join(session_id)
    }

    pub fn session_note_dir(&self, session_id: &str) -> PathBuf {
        self.session_root(session_id).join(NOTE_DIR)
    }

    pub fn session_topic_path(&self, session_id: &str, topic: &str) -> PathBuf {
        self.session_note_dir(session_id)
            .join(format!("{}.md", topic))
    }

    pub fn session_state_path(&self, session_id: &str) -> PathBuf {
        self.session_root(session_id).join("state.json")
    }

    pub fn scopes_root(&self) -> PathBuf {
        self.root.join(SCOPES_DIR)
    }

    pub fn global_root(&self) -> PathBuf {
        self.scopes_root().join(GLOBAL_DIR)
    }

    pub fn project_root(&self, project_key: &str) -> PathBuf {
        if self
            .project_id
            .as_ref()
            .is_some_and(|project_id| project_id.as_str() == project_key)
        {
            return ProjectMemoryPathResolver::from_data_dir(self.data_dir.clone())
                .memory_root(self.project_id.as_ref().expect("checked Project id"));
        }
        self.scopes_root().join(PROJECTS_DIR).join(project_key)
    }

    pub fn scope_root(&self, scope: MemoryScope, project_key: Option<&str>) -> PathBuf {
        match scope {
            MemoryScope::Global => self.global_root(),
            MemoryScope::Project => self.project_root(project_key.unwrap_or("unknown")),
            MemoryScope::Session => self.sessions_root(),
        }
    }

    pub fn topic_dir(&self, scope: MemoryScope, project_key: Option<&str>) -> PathBuf {
        match scope {
            MemoryScope::Global => self.global_root().join(TOPICS_DIR),
            MemoryScope::Project => self
                .project_root(project_key.unwrap_or("unknown"))
                .join(TOPICS_DIR),
            MemoryScope::Session => self.sessions_root(),
        }
    }

    pub fn topic_path(
        &self,
        scope: MemoryScope,
        project_key: Option<&str>,
        memory_id: &str,
    ) -> PathBuf {
        self.topic_dir(scope, project_key)
            .join(format!("{}.md", memory_id))
    }

    pub fn indexes_dir(&self, scope: MemoryScope, project_key: Option<&str>) -> PathBuf {
        match scope {
            MemoryScope::Global => self.global_root().join(INDEXES_DIR),
            MemoryScope::Project => self
                .project_root(project_key.unwrap_or("unknown"))
                .join(INDEXES_DIR),
            MemoryScope::Session => self.sessions_root(),
        }
    }

    pub fn views_dir(&self, scope: MemoryScope, project_key: Option<&str>) -> PathBuf {
        match scope {
            MemoryScope::Global => self.global_root().join(VIEWS_DIR),
            MemoryScope::Project => self
                .project_root(project_key.unwrap_or("unknown"))
                .join(VIEWS_DIR),
            MemoryScope::Session => self.sessions_root(),
        }
    }

    pub fn logs_dir(&self, scope: MemoryScope, project_key: Option<&str>) -> PathBuf {
        match scope {
            MemoryScope::Global => self.global_root().join(LOGS_DIR),
            MemoryScope::Project => self
                .project_root(project_key.unwrap_or("unknown"))
                .join(LOGS_DIR),
            MemoryScope::Session => self.sessions_root(),
        }
    }

    pub fn state_dir(&self, scope: MemoryScope, project_key: Option<&str>) -> PathBuf {
        match scope {
            MemoryScope::Global => self.global_root().join(STATE_DIR),
            MemoryScope::Project => self
                .project_root(project_key.unwrap_or("unknown"))
                .join(STATE_DIR),
            MemoryScope::Session => self.sessions_root(),
        }
    }
}

fn infer_data_dir_from_root(root: &Path) -> PathBuf {
    if !root
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value == MEMORY_VERSION_DIR)
    {
        return root.to_path_buf();
    }
    let Some(memory_dir) = root.parent() else {
        return root.to_path_buf();
    };
    if !memory_dir
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value == MEMORY_ROOT_DIR)
    {
        return root.to_path_buf();
    }
    memory_dir.parent().unwrap_or(root).to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_follow_v1_layout() {
        let resolver = MemoryPathResolver::new("/tmp/memory/v1");
        assert_eq!(
            resolver.session_topic_path("session-1", "default"),
            PathBuf::from("/tmp/memory/v1/sessions/session-1/note/default.md")
        );
        assert_eq!(
            resolver.topic_path(MemoryScope::Project, Some("proj-1"), "mem_1"),
            PathBuf::from("/tmp/memory/v1/scopes/projects/proj-1/topics/mem_1.md")
        );
    }

    #[test]
    fn typed_project_memory_uses_caller_supplied_project_home() {
        let resolver = ProjectMemoryPathResolver::from_data_dir("/tmp/jiandu");
        let project_id = ProjectId::parse("01JABCDEF0123456789ABCDEFG").expect("id");
        assert_eq!(
            resolver.memory_root(&project_id),
            PathBuf::from("/tmp/jiandu/projects/01JABCDEF0123456789ABCDEFG/memory/v1")
        );
        let scoped = MemoryPathResolver::from_data_dir("/tmp/jiandu").for_project(&project_id);
        assert_eq!(
            scoped.topic_dir(MemoryScope::Project, Some(project_id.as_str())),
            PathBuf::from("/tmp/jiandu/projects/01JABCDEF0123456789ABCDEFG/memory/v1/topics")
        );
        for invalid in ["", "../escape", "with/slash", "with space"] {
            assert!(ProjectId::parse(invalid).is_err());
        }
    }
}
