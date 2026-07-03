//! `ScopedMemoryClient` — partition-gated memory access.
//!
//! The permission contract lives inside the client: `read`/`write` reject any
//! partition not in the plugin's manifest-declared lists. P5 (user model) is
//! never plugin-writable.

use std::collections::HashSet;

use crate::access_gate::PluginAccessGate;
use crate::error::{PluginError, PluginResult};
use crate::manifest::PluginManifest;
use crate::memory_spec::{MemoryItem, MemoryQuery, Partition};

pub trait MemoryHost: Send + Sync {
    fn host_read(
        &self,
        partition: Partition,
        query: &MemoryQuery,
    ) -> Result<Vec<MemoryItem>, String>;
    fn host_write(&mut self, partition: Partition, item: MemoryItem) -> Result<(), String>;
}

pub struct ScopedMemoryClient<'a> {
    plugin_name: String,
    readable: HashSet<Partition>,
    writable: HashSet<Partition>,
    host: &'a mut dyn MemoryHost,
}

impl<'a> ScopedMemoryClient<'a> {
    pub fn new(manifest: &PluginManifest, host: &'a mut dyn MemoryHost) -> PluginResult<Self> {
        PluginAccessGate::require_memory_access(manifest)?;
        let readable = parse_set(&manifest.permissions.memory_partitions_readable);
        let writable = parse_set(&manifest.permissions.memory_partitions_writable);
        Ok(Self {
            plugin_name: manifest.plugin.name.clone(),
            readable,
            writable,
            host,
        })
    }

    pub fn read(&self, partition: Partition, query: &MemoryQuery) -> PluginResult<Vec<MemoryItem>> {
        if !self.readable.contains(&partition) {
            return Err(PluginError::PermissionDenied {
                plugin: self.plugin_name.clone(),
                operation: format!("memory.read({})", partition.as_str()),
            });
        }
        self.host
            .host_read(partition, query)
            .map_err(|e| PluginError::PermissionDenied {
                plugin: self.plugin_name.clone(),
                operation: format!("memory.read({}): {e}", partition.as_str()),
            })
    }

    pub fn write(&mut self, partition: Partition, item: MemoryItem) -> PluginResult<()> {
        if !self.writable.contains(&partition) {
            return Err(PluginError::PermissionDenied {
                plugin: self.plugin_name.clone(),
                operation: format!("memory.write({})", partition.as_str()),
            });
        }
        self.host
            .host_write(partition, item)
            .map_err(|e| PluginError::PermissionDenied {
                plugin: self.plugin_name.clone(),
                operation: format!("memory.write({}): {e}", partition.as_str()),
            })
    }
}

fn parse_set(list: &[String]) -> HashSet<Partition> {
    list.iter()
        .filter_map(|s| match s.as_str() {
            "P1" => Some(Partition::P1),
            "P2" => Some(Partition::P2),
            "P3" => Some(Partition::P3),
            "P4" => Some(Partition::P4),
            "P5" => Some(Partition::P5),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inert host — the gate must reject (or admit) before any host call.
    struct NoopHost;
    impl MemoryHost for NoopHost {
        fn host_read(
            &self,
            _partition: Partition,
            _query: &MemoryQuery,
        ) -> Result<Vec<MemoryItem>, String> {
            Ok(Vec::new())
        }
        fn host_write(&mut self, _partition: Partition, _item: MemoryItem) -> Result<(), String> {
            Ok(())
        }
    }

    fn manifest(toml: &str) -> PluginManifest {
        PluginManifest::from_toml_str(toml).unwrap()
    }

    const HEADER: &str = r#"
[plugin]
name = "genesis-mem"
version = "1.0.0"
description = "t"
entry = "builtin:m"
authors = ["t"]
license = "MIT"
[permissions]
"#;

    #[test]
    fn new_denies_manifest_without_memory_partitions() {
        let m = manifest(HEADER);
        let mut host = NoopHost;
        let err = match ScopedMemoryClient::new(&m, &mut host) {
            Err(e) => e,
            Ok(_) => panic!("expected PermissionDenied when no partitions are declared"),
        };
        match err {
            PluginError::PermissionDenied { plugin, operation } => {
                assert_eq!(plugin, "genesis-mem");
                assert_eq!(operation, "memory_access");
            }
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
    }

    #[test]
    fn new_allows_manifest_with_readable_partition() {
        let m = manifest(&format!("{HEADER}memory_partitions_readable = [\"P2\"]\n"));
        let mut host = NoopHost;
        let client =
            ScopedMemoryClient::new(&m, &mut host).expect("readable partition grants access");
        assert!(client.readable.contains(&Partition::P2));
        assert!(client.writable.is_empty());
    }

    #[test]
    fn new_allows_manifest_with_writable_partition() {
        let m = manifest(&format!("{HEADER}memory_partitions_writable = [\"P2\"]\n"));
        let mut host = NoopHost;
        let client =
            ScopedMemoryClient::new(&m, &mut host).expect("writable partition grants access");
        assert!(client.writable.contains(&Partition::P2));
        assert!(client.readable.is_empty());
    }
}
