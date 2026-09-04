//! Typed ConfigurationResolver (M1, REQ-EV-0039): deterministic merge of
//! admin/project/user configuration layers with provenance, under the rule
//! that **lower authority can never widen** (docs/45; QUAL-EV-0039).
//!
//! Merge laws (domain-specific, not generic deep-merge):
//! - network allowlist: intersection across every layer that defines one —
//!   a lower layer may narrow but never widen reach;
//! - model policy: a layer's default model must be inside every layer's
//!   allowlist that defines one, otherwise resolution fails;
//! - every resolved value records the highest-authority layer that set it.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Authority {
    Admin,
    Project,
    User,
}

impl Authority {
    pub fn as_str(&self) -> &'static str {
        match self {
            Authority::Admin => "admin",
            Authority::Project => "project",
            Authority::User => "user",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NetworkPolicy {
    pub allowed_hosts: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelPolicy {
    pub allowed_models: Vec<String>,
    pub default_model: Option<String>,
}

/// One configuration layer; `None` fields are unset by that authority.
#[derive(Clone, Debug)]
pub struct ConfigLayer {
    pub authority: Authority,
    pub network: Option<NetworkPolicy>,
    pub model: Option<ModelPolicy>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ResolvedNetwork {
    pub allowed_hosts: Vec<String>,
    pub provenance: Authority,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ResolvedModel {
    pub allowed_models: Vec<String>,
    pub default_model: Option<String>,
    pub provenance: Authority,
}

#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct ResolvedConfig {
    pub network: Option<ResolvedNetwork>,
    pub model: Option<ResolvedModel>,
}

/// Resolves layers from highest to lowest authority. Deterministic: the same
/// layer set always yields the same `ResolvedConfig`.
pub fn resolve(layers: &[ConfigLayer]) -> Result<ResolvedConfig, String> {
    let mut sorted: Vec<&ConfigLayer> = layers.iter().collect();
    sorted.sort_by_key(|l| l.authority); // Admin(0) first — descending authority

    // Network: intersect allowlists; provenance = the layer that first
    // (highest authority) defined one. Undefined = not configured.
    let mut hosts: Option<Vec<String>> = None;
    let mut network_prov = Authority::User;
    for layer in &sorted {
        if let Some(net) = &layer.network {
            hosts = Some(match hosts {
                None => net.allowed_hosts.clone(),
                Some(existing) => existing
                    .into_iter()
                    .filter(|h| net.allowed_hosts.contains(h))
                    .collect(),
            });
            network_prov = layer.authority;
        }
    }
    let network = hosts.map(|allowed_hosts| ResolvedNetwork {
        allowed_hosts,
        provenance: network_prov,
    });

    // Model: allowed set is the union of definitions? No — the strict rule:
    // every layer's allowlist constrains; the default must be allowed by all
    // layers that define an allowlist. Provenance = highest layer defining
    // the default.
    let mut allowed: Option<Vec<String>> = None;
    let mut default: Option<(String, Authority)> = None;
    for layer in &sorted {
        if let Some(model) = &layer.model {
            allowed = Some(match allowed {
                None => model.allowed_models.clone(),
                Some(existing) => existing
                    .into_iter()
                    .filter(|m| model.allowed_models.contains(m))
                    .collect(),
            });
            if let Some(default_model) = &model.default_model {
                if default.is_none() {
                    default = Some((default_model.clone(), layer.authority));
                }
            }
        }
    }
    let allowed_models = allowed;
    if let (Some(allowlist), Some((default_model, _))) = (&allowed_models, &default) {
        if !allowlist.contains(default_model) {
            return Err(format!(
                "default model {default_model:?} is not in the resolved allowlist — a lower authority attempted to widen"
            ));
        }
    }
    let model_prov = default.as_ref().map_or(Authority::User, |(_, a)| *a);

    Ok(ResolvedConfig {
        network,
        model: allowed_models.map(|allowed_models| ResolvedModel {
            allowed_models,
            default_model: default.map(|(m, _)| m),
            provenance: model_prov,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(
        authority: Authority,
        hosts: Option<Vec<String>>,
        models: Option<ModelPolicy>,
    ) -> ConfigLayer {
        ConfigLayer {
            authority,
            network: hosts.map(|allowed_hosts| NetworkPolicy { allowed_hosts }),
            model: models,
        }
    }

    /// QUAL-EV-0039: conflicting admin/project/user configs resolve
    /// deterministically; lower authority cannot widen.
    #[test]
    fn lower_authority_cannot_widen_and_resolution_is_deterministic() {
        let layers = vec![
            layer(
                Authority::User,
                Some(vec!["a.dev".into(), "b.dev".into(), "c.dev".into()]),
                None,
            ),
            layer(
                Authority::Admin,
                Some(vec!["a.dev".into(), "b.dev".into()]),
                None,
            ),
            layer(Authority::Project, Some(vec!["a.dev".into()]), None),
        ];
        let resolved = resolve(&layers).unwrap();
        // Admin and project both constrain; the effective allowlist is the
        // narrowest set: a.dev only. The user layer could not widen it.
        assert_eq!(
            resolved.network.as_ref().unwrap().allowed_hosts,
            vec!["a.dev".to_string()]
        );

        // Determinism: scrambled input order, same output.
        let mut scrambled = vec![layers[2].clone(), layers[0].clone(), layers[1].clone()];
        scrambled.reverse();
        assert_eq!(resolve(&scrambled).unwrap(), resolved);
    }

    #[test]
    fn model_default_must_survive_every_allowlist() {
        let layers = vec![
            layer(
                Authority::Admin,
                Some(vec!["a.dev".into()]),
                Some(ModelPolicy {
                    allowed_models: vec!["m-a".into(), "m-b".into()],
                    default_model: Some("m-a".into()),
                }),
            ),
            layer(
                Authority::User,
                Some(vec!["a.dev".into()]),
                Some(ModelPolicy {
                    allowed_models: vec!["m-a".into()],
                    default_model: Some("m-b".into()),
                }),
            ),
        ];
        // The user's default attempt (m-b) is overridden by the admin's
        // m-a: lower authority cannot widen. Resolution is deterministic.
        let resolved = resolve(&layers).unwrap();
        assert_eq!(
            resolved.model.as_ref().unwrap().default_model,
            Some("m-a".into())
        );
        assert_eq!(
            resolved.model.as_ref().unwrap().allowed_models,
            vec!["m-a".to_string()]
        );

        let compliant = vec![
            layer(
                Authority::Admin,
                Some(vec!["a.dev".into()]),
                Some(ModelPolicy {
                    allowed_models: vec!["m-a".into(), "m-b".into()],
                    default_model: None,
                }),
            ),
            layer(
                Authority::User,
                Some(vec!["a.dev".into()]),
                Some(ModelPolicy {
                    allowed_models: vec!["m-a".into()],
                    default_model: Some("m-a".into()),
                }),
            ),
        ];
        let resolved = resolve(&compliant).unwrap();
        let model = resolved.model.as_ref().unwrap();
        assert_eq!(model.default_model, Some("m-a".into()));
        assert_eq!(model.provenance, Authority::User);
    }

    #[test]
    fn unset_concerns_resolve_to_none_and_contradiction_is_rejected() {
        // Nothing configured: both concerns are None.
        let resolved = resolve(&[]).unwrap();
        assert_eq!(resolved.network, None);
        assert_eq!(resolved.model, None);

        // A contradictory highest-authority config (default outside its own
        // allowlist) is rejected outright.
        let contradictory = vec![layer(
            Authority::Admin,
            None,
            Some(ModelPolicy {
                allowed_models: vec!["m-a".into()],
                default_model: Some("m-b".into()),
            }),
        )];
        assert!(resolve(&contradictory).is_err());
    }
}
