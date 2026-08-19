//! Persistent CAD face / region identity and reimport remap classification.
//!
//! Triangle order and heuristic `component-{index}` labels are not stable
//! identity. Boundary assignments that will become internal-flow evidence must
//! carry a non-heuristic face identity and must hard-reject ambiguous remaps
//! after geometry reimport.

use serde::{Deserialize, Serialize};

/// How a region candidate was identified on the imported source.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FaceIdentityKind {
    /// No face identity was recovered from the source.
    #[default]
    Absent,
    /// Order- or index-based placeholder (for example `component-0`). Not
    /// evidence-safe for boundary conditions.
    HeuristicIndex,
    /// STEP entity id from the Part 21 graph (exporter-local, not CAx-IF persistent).
    StepEntityId,
    /// `ADVANCED_FACE` name attribute when the exporter populated it.
    StepAdvancedFaceName,
    /// Face id emitted by the out-of-process CAD bridge tessellation.
    BridgeFaceId,
}

impl FaceIdentityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::HeuristicIndex => "heuristic_index",
            Self::StepEntityId => "step_entity_id",
            Self::StepAdvancedFaceName => "step_advanced_face_name",
            Self::BridgeFaceId => "bridge_face_id",
        }
    }

    /// Identity kinds that may back immutable boundary assignments.
    pub fn is_evidence_safe(self) -> bool {
        matches!(
            self,
            Self::StepEntityId | Self::StepAdvancedFaceName | Self::BridgeFaceId
        )
    }
}

/// One face/region identity attached to a named assignment or tessellated face.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FaceIdentity {
    pub kind: FaceIdentityKind,
    pub stable_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl FaceIdentity {
    pub fn new(kind: FaceIdentityKind, stable_id: impl Into<String>) -> Self {
        Self {
            kind,
            stable_id: stable_id.into(),
            signature: None,
        }
    }

    pub fn heuristic_index(index: usize) -> Self {
        Self::new(
            FaceIdentityKind::HeuristicIndex,
            format!("component-{index}"),
        )
    }

    pub fn is_evidence_safe(&self) -> bool {
        self.kind.is_evidence_safe() && !self.stable_id.trim().is_empty()
    }
}

/// Classification of one named region across a reimport.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemapClass {
    Preserved,
    Changed,
    Added,
    Removed,
    Ambiguous,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RegionRemapEntry {
    pub name: String,
    pub role: String,
    pub class: RemapClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_id: Option<String>,
    pub detail: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct RegionRemapReport {
    pub entries: Vec<RegionRemapEntry>,
}

impl RegionRemapReport {
    pub fn has_ambiguous(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.class == RemapClass::Ambiguous)
    }

    pub fn has_removed_assigned(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.class == RemapClass::Removed && !entry.role.is_empty())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedRegionIdentity {
    pub name: String,
    pub role: String,
    pub identity: FaceIdentity,
}

/// Diff previous vs next named-region identities without using triangle order.
///
/// Matching is by evidence-safe `stable_id` when both sides have one. Heuristic
/// or absent identities never count as preserved across a reimport.
pub fn diff_named_region_identities(
    previous: &[NamedRegionIdentity],
    next: &[NamedRegionIdentity],
) -> RegionRemapReport {
    let mut entries = Vec::new();
    let mut consumed_next = vec![false; next.len()];

    for prior in previous {
        if !prior.identity.is_evidence_safe() {
            entries.push(RegionRemapEntry {
                name: prior.name.clone(),
                role: prior.role.clone(),
                class: RemapClass::Ambiguous,
                previous_id: Some(prior.identity.stable_id.clone()),
                next_id: None,
                detail: format!(
                    "previous identity kind {} is not evidence-safe; refusing silent remap",
                    prior.identity.kind.as_str()
                ),
            });
            continue;
        }

        let matches: Vec<usize> = next
            .iter()
            .enumerate()
            .filter(|(index, candidate)| {
                !consumed_next[*index]
                    && candidate.identity.is_evidence_safe()
                    && candidate.identity.stable_id == prior.identity.stable_id
            })
            .map(|(index, _)| index)
            .collect();

        match matches.as_slice() {
            [] => entries.push(RegionRemapEntry {
                name: prior.name.clone(),
                role: prior.role.clone(),
                class: RemapClass::Removed,
                previous_id: Some(prior.identity.stable_id.clone()),
                next_id: None,
                detail: "stable face identity is absent after reimport".into(),
            }),
            [index] => {
                consumed_next[*index] = true;
                let candidate = &next[*index];
                let class = if candidate.role == prior.role && candidate.name == prior.name {
                    RemapClass::Preserved
                } else {
                    RemapClass::Changed
                };
                entries.push(RegionRemapEntry {
                    name: prior.name.clone(),
                    role: prior.role.clone(),
                    class,
                    previous_id: Some(prior.identity.stable_id.clone()),
                    next_id: Some(candidate.identity.stable_id.clone()),
                    detail: if class == RemapClass::Preserved {
                        "stable face identity preserved".into()
                    } else {
                        format!(
                            "stable face identity preserved but label/role changed ({} / {} → {} / {})",
                            prior.name, prior.role, candidate.name, candidate.role
                        )
                    },
                });
            }
            _ => entries.push(RegionRemapEntry {
                name: prior.name.clone(),
                role: prior.role.clone(),
                class: RemapClass::Ambiguous,
                previous_id: Some(prior.identity.stable_id.clone()),
                next_id: None,
                detail: format!(
                    "{} faces share stable id {}; refusing silent remap",
                    matches.len(),
                    prior.identity.stable_id
                ),
            }),
        }
    }

    for (index, candidate) in next.iter().enumerate() {
        if consumed_next[index] {
            continue;
        }
        if !candidate.identity.is_evidence_safe() {
            entries.push(RegionRemapEntry {
                name: candidate.name.clone(),
                role: candidate.role.clone(),
                class: RemapClass::Ambiguous,
                previous_id: None,
                next_id: Some(candidate.identity.stable_id.clone()),
                detail: format!(
                    "new identity kind {} is not evidence-safe",
                    candidate.identity.kind.as_str()
                ),
            });
            continue;
        }
        entries.push(RegionRemapEntry {
            name: candidate.name.clone(),
            role: candidate.role.clone(),
            class: RemapClass::Added,
            previous_id: None,
            next_id: Some(candidate.identity.stable_id.clone()),
            detail: "new stable face identity after reimport".into(),
        });
    }

    RegionRemapReport { entries }
}

/// Fail closed when a reimport would silently remap or drop assigned regions.
pub fn validate_region_remap_for_evidence(report: &RegionRemapReport) -> Result<(), String> {
    if report.has_ambiguous() {
        let detail = report
            .entries
            .iter()
            .filter(|entry| entry.class == RemapClass::Ambiguous)
            .map(|entry| entry.detail.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!(
            "CAD face identity remap is ambiguous; boundary assignments were not updated ({detail})"
        ));
    }
    if report.has_removed_assigned() {
        let names = report
            .entries
            .iter()
            .filter(|entry| entry.class == RemapClass::Removed && !entry.role.is_empty())
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "CAD face identity for assigned region(s) [{names}] disappeared on reimport; \
             refuse silent remap"
        ));
    }
    Ok(())
}

/// Reject drafting an evidence-bound region on a non-stable candidate.
pub fn validate_assignment_identity(identity: &FaceIdentity) -> Result<(), String> {
    if identity.is_evidence_safe() {
        Ok(())
    } else {
        Err(format!(
            "region assignment requires a persistent CAD face identity; got {} ({})",
            identity.kind.as_str(),
            if identity.stable_id.is_empty() {
                "empty id"
            } else {
                identity.stable_id.as_str()
            }
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(name: &str, role: &str, kind: FaceIdentityKind, id: &str) -> NamedRegionIdentity {
        NamedRegionIdentity {
            name: name.into(),
            role: role.into(),
            identity: FaceIdentity::new(kind, id),
        }
    }

    #[test]
    fn heuristic_identities_are_never_preserved() {
        let previous = [region(
            "inlet",
            "inlet",
            FaceIdentityKind::HeuristicIndex,
            "component-0",
        )];
        let next = [region(
            "inlet",
            "inlet",
            FaceIdentityKind::HeuristicIndex,
            "component-0",
        )];
        let report = diff_named_region_identities(&previous, &next);
        assert!(report.has_ambiguous());
        assert!(validate_region_remap_for_evidence(&report)
            .unwrap_err()
            .contains("ambiguous"));
    }

    #[test]
    fn bridge_face_ids_preserve_across_reimport() {
        let previous = [region(
            "wall",
            "wall",
            FaceIdentityKind::BridgeFaceId,
            "face:17",
        )];
        let next = [region(
            "wall",
            "wall",
            FaceIdentityKind::BridgeFaceId,
            "face:17",
        )];
        let report = diff_named_region_identities(&previous, &next);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].class, RemapClass::Preserved);
        validate_region_remap_for_evidence(&report).unwrap();
    }

    #[test]
    fn duplicate_stable_ids_are_ambiguous() {
        let previous = [region("a", "wall", FaceIdentityKind::StepEntityId, "#42")];
        let next = [
            region("a", "wall", FaceIdentityKind::StepEntityId, "#42"),
            region("b", "wall", FaceIdentityKind::StepEntityId, "#42"),
        ];
        let report = diff_named_region_identities(&previous, &next);
        assert!(report.has_ambiguous());
    }

    #[test]
    fn removed_assigned_region_fails_closed() {
        let previous = [region(
            "inlet",
            "inlet",
            FaceIdentityKind::StepAdvancedFaceName,
            "INLET_FACE",
        )];
        let report = diff_named_region_identities(&previous, &[]);
        assert!(validate_region_remap_for_evidence(&report)
            .unwrap_err()
            .contains("disappeared"));
    }

    #[test]
    fn evidence_assignment_rejects_absent_identity() {
        let identity = FaceIdentity::new(FaceIdentityKind::Absent, "");
        assert!(validate_assignment_identity(&identity)
            .unwrap_err()
            .contains("persistent CAD face identity"));
    }
}
