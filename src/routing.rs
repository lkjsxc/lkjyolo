//! Bind each request to the authenticated Coder user's own workspace.
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedWorkspaces {
    #[serde(default = "default_workspace")]
    pub preferred_name: String,
    /// Immutable Coder user ID -> workspace name, never another owner's path.
    #[serde(default)]
    pub overrides: BTreeMap<String, String>,
}
fn default_workspace() -> String {
    "lkjyolo".into()
}

pub enum Routing {
    Single { owner_id: String, workspace: String },
    Owned(OwnedWorkspaces),
}
#[derive(Debug)]
pub struct Identity {
    pub id: String,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
impl Routing {
    pub fn new(
        owner_id: Option<String>,
        workspace: Option<String>,
        owned: Option<OwnedWorkspaces>,
    ) -> Result<Self> {
        match (owner_id, workspace, owned) {
            (Some(owner_id), Some(workspace), None) => {
                ensure!(
                    !owner_id.is_empty()
                        && workspace.split('/').count() == 2
                        && workspace.split('/').all(valid_name),
                    "Set an exact owner ID and owner/workspace"
                );
                Ok(Self::Single {
                    owner_id,
                    workspace,
                })
            }
            (None, None, Some(owned)) => {
                ensure!(
                    valid_name(&owned.preferred_name),
                    "Invalid preferred workspace name"
                );
                for (id, name) in &owned.overrides {
                    uuid::Uuid::parse_str(id)
                        .context("Workspace override requires a Coder user UUID")?;
                    ensure!(
                        valid_name(name),
                        "Workspace overrides must be names, not owner paths or agent selectors"
                    );
                }
                Ok(Self::Owned(owned))
            }
            _ => bail!(
                "Choose either owner_id + workspace or owned_workspaces; no implicit access expansion"
            ),
        }
    }
    pub fn identity(&self, body: &Value) -> Option<Identity> {
        if body["status"] != "active" {
            return None;
        }
        let id = body["id"].as_str()?.to_owned();
        if id.is_empty() {
            return None;
        }
        match self {
            Self::Single { owner_id, .. } if *owner_id == id => Some(Identity { id }),
            Self::Owned(_) if uuid::Uuid::parse_str(&id).is_ok() => Some(Identity { id }),
            _ => None,
        }
    }
    pub fn select(&self, identity: &Identity, workspaces: &Value) -> Result<String> {
        let Self::Owned(config) = self else {
            bail!("Owned routing required");
        };
        let workspaces = workspaces
            .as_array()
            .context("Invalid Coder workspace list")?;
        let preferred = config
            .overrides
            .get(&identity.id)
            .unwrap_or(&config.preferred_name);
        let matches: Vec<_> = workspaces
            .iter()
            .filter(|w| w["name"] == *preferred)
            .collect();
        let selected = match matches.as_slice() {
            [one] => *one,
            [] if config.overrides.contains_key(&identity.id) => bail!(
                "Configured workspace is missing; ask the administrator to update your workspace mapping"
            ),
            [] if workspaces.len() == 1 => &workspaces[0],
            [] if workspaces.is_empty() => bail!(
                "No workspace exists for your account. Open Coder, create your own workspace named lkjyolo, then retry"
            ),
            _ => bail!(
                "Workspace selection is ambiguous. Name the workspace you want to use lkjyolo, or ask the administrator to set your mapping"
            ),
        };
        let id = selected["id"].as_str().context("Workspace ID missing")?;
        uuid::Uuid::parse_str(id).context("Invalid workspace ID")?;
        Ok(id.into())
    }
}

/// Check the full object independently: Coder admins can see other users' data.
/// The immutable workspace ID is subsequently passed to execution, avoiding a
/// name-based ownership race if someone renames or recreates a workspace.
pub fn verify_owner(identity: &Identity, expected: &str, workspace: &Value) -> Result<()> {
    ensure!(
        workspace["id"] == expected
            && workspace["owner_id"] == identity.id
            && workspace["deleted"] != true,
        "Workspace ownership verification failed; no command was run"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const ALICE: &str = "10000000-0000-0000-0000-000000000001";
    const BOB: &str = "10000000-0000-0000-0000-000000000002";
    const W1: &str = "20000000-0000-0000-0000-000000000001";
    const W2: &str = "20000000-0000-0000-0000-000000000002";
    fn owned() -> Routing {
        Routing::new(None, None, Some(serde_json::from_value(json!({})).unwrap())).unwrap()
    }
    fn alice() -> Identity {
        Identity { id: ALICE.into() }
    }
    #[test]
    fn configuration_never_silently_widens_access() {
        assert!(Routing::new(None, None, None).is_err());
        assert!(Routing::new(Some(ALICE.into()), None, None).is_err());
        assert!(
            Routing::new(
                Some(ALICE.into()),
                Some("alice/main".into()),
                Some(serde_json::from_value(json!({})).unwrap())
            )
            .is_err()
        );
        for name in ["bob/main", "main.agent", "../main", "", "a b"] {
            let config = json!({"overrides":{ALICE:name}});
            assert!(
                Routing::new(None, None, Some(serde_json::from_value(config).unwrap())).is_err()
            );
        }
    }
    #[test]
    fn active_users_only_and_legacy_owner_preserved() {
        let old = Routing::new(Some(ALICE.into()), Some("alice/main".into()), None).unwrap();
        assert!(
            old.identity(&json!({"id":ALICE,"status":"active"}))
                .is_some()
        );
        assert!(old.identity(&json!({"id":BOB,"status":"active"})).is_none());
        assert!(
            owned()
                .identity(&json!({"id":BOB,"status":"active"}))
                .is_some()
        );
        for status in ["suspended", "dormant", ""] {
            assert!(
                owned()
                    .identity(&json!({"id":ALICE,"status":status}))
                    .is_none()
            );
        }
        assert!(owned().identity(&json!({"status":"active"})).is_none());
    }
    #[test]
    fn selection_is_deterministic_and_never_uses_an_arbitrary_workspace() {
        let routing = owned();
        assert!(routing.select(&alice(), &json!([])).is_err());
        assert_eq!(
            routing
                .select(&alice(), &json!([{"id":W1,"name":"existing"}]))
                .unwrap(),
            W1
        );
        let multiple = json!([{"id":W1,"name":"existing"},{"id":W2,"name":"lkjyolo"}]);
        assert_eq!(routing.select(&alice(), &multiple).unwrap(), W2);
        assert!(
            routing
                .select(
                    &alice(),
                    &json!([{"id":W1,"name":"a"},{"id":W2,"name":"b"}])
                )
                .is_err()
        );
        assert!(
            routing
                .select(
                    &alice(),
                    &json!([{"id":W1,"name":"lkjyolo"},{"id":W2,"name":"lkjyolo"}])
                )
                .is_err()
        );
    }
    #[test]
    fn overrides_are_bound_to_immutable_user_ids() {
        let routing = Routing::new(
            None,
            None,
            Some(serde_json::from_value(json!({"overrides":{ALICE:"existing"}})).unwrap()),
        )
        .unwrap();
        let list = json!([{"id":W1,"name":"existing"},{"id":W2,"name":"lkjyolo"}]);
        assert_eq!(routing.select(&alice(), &list).unwrap(), W1);
        assert_eq!(
            routing.select(&Identity { id: BOB.into() }, &list).unwrap(),
            W2
        );
        assert!(
            routing
                .select(&alice(), &json!([{"id":W2,"name":"lkjyolo"}]))
                .is_err()
        );
    }
    #[test]
    fn foreign_deleted_and_replaced_workspaces_fail_closed() {
        assert!(verify_owner(&alice(), W1, &json!({"id":W1,"owner_id":ALICE})).is_ok());
        for body in [
            json!({"id":W1,"owner_id":BOB}),
            json!({"id":W2,"owner_id":ALICE}),
            json!({"id":W1,"owner_id":ALICE,"deleted":true}),
            json!({"id":W1}),
        ] {
            assert!(verify_owner(&alice(), W1, &body).is_err());
        }
    }
}
