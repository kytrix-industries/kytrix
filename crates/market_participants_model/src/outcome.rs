use std::collections::BTreeSet;

use ahash::{AHashMap, AHashSet};
use serde::{Deserialize, Deserializer, Serialize, de};

use crate::{
    asset::AssetId,
    decimal::{Fraction, PositiveRatio, UnixMillis},
    error::{
        ModelError, SettlementAssetRole, require_non_negative_time, require_ordered_time,
        require_sorted_and_unique,
    },
    identifier::{
        CanonicalEncoder, CanonicalTerms, IdentifierValue, IdentityText, OutcomeEventIdentity,
        OutcomeGroupIdentity, OutcomeMemberIdentity, OutcomeRiskPoolIdentity, OutcomeSideIdentity,
        ResolutionSourceId,
    },
    settlement::{Expiry, ExpiryWire, LineageRelation, validate_lineage},
};

crate::identifier::identity_type!(OutcomeEventId, OutcomeEventIdentity);
crate::identifier::identity_type!(OutcomeGroupId, OutcomeGroupIdentity);
crate::identifier::identity_type!(OutcomeMemberId, OutcomeMemberIdentity);
crate::identifier::identity_type!(OutcomeSideId, OutcomeSideIdentity);
crate::identifier::identity_type!(OutcomeRiskPoolId, OutcomeRiskPoolIdentity);

pub const MAX_OUTCOME_QUESTION_SCALARS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SelectionCardinality {
    #[serde(rename = "at_most_one")]
    AtMostOne,
    #[serde(rename = "exactly_one")]
    ExactlyOne,
}

impl SelectionCardinality {
    pub const fn label(self) -> &'static str {
        match self {
            Self::AtMostOne => "at-most-one",
            Self::ExactlyOne => "exactly-one",
        }
    }
}

impl CanonicalTerms for SelectionCardinality {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label(self.label());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "cardinality")]
pub enum OutcomeTopology {
    #[serde(rename = "binary")]
    Binary,
    #[serde(rename = "mutually_exclusive")]
    MutuallyExclusive(SelectionCardinality),
}

impl OutcomeTopology {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::MutuallyExclusive(_) => "mutually-exclusive",
        }
    }

    pub const fn cardinality(self) -> Option<SelectionCardinality> {
        match self {
            Self::MutuallyExclusive(cardinality) => Some(cardinality),
            Self::Binary => None,
        }
    }
}

impl CanonicalTerms for OutcomeTopology {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        match self {
            Self::Binary => encoder.write_label("binary"),
            Self::MutuallyExclusive(cardinality) => {
                encoder.write_label("mutually-exclusive");
                encoder.write_terms(cardinality);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SettlementFractionRule {
    #[serde(rename = "binary")]
    Binary,
    #[serde(rename = "any_fraction")]
    AnyFraction,
}

impl SettlementFractionRule {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::AnyFraction => "any-fraction",
        }
    }

    pub fn validate_fraction(
        &self,
        group: &OutcomeGroupId,
        fraction: Fraction,
    ) -> Result<(), ModelError> {
        if *self == Self::Binary && !fraction.is_binary() {
            return Err(ModelError::OutcomeSettlementFractionInvalid {
                group: *group,
                fraction_kind: "binary",
            });
        }
        Ok(())
    }
}

impl CanonicalTerms for SettlementFractionRule {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label(self.label());
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "rule")]
pub enum ResolutionRule {
    #[serde(rename = "fixed_at")]
    FixedAt {
        #[serde(rename = "at")]
        at: UnixMillis,
    },
    #[serde(rename = "external")]
    External {
        #[serde(rename = "source")]
        source: ResolutionSourceId,
        #[serde(rename = "deadline")]
        deadline: UnixMillis,
    },
}

impl ResolutionRule {
    pub fn validate(&self, expiry: &Expiry) -> Result<(), ModelError> {
        match self {
            Self::FixedAt { at } => {
                require_non_negative_time(*at, "resolution.at")?;
                require_ordered_time(
                    *at,
                    expiry.expiration_at(),
                    "resolution.at",
                    "expiration_at",
                )
            }
            Self::External { deadline, .. } => {
                require_non_negative_time(*deadline, "resolution.deadline")?;
                require_ordered_time(
                    *deadline,
                    expiry.expiration_at(),
                    "resolution.deadline",
                    "expiration_at",
                )
            }
        }
    }

    pub fn deadline(&self) -> UnixMillis {
        match self {
            Self::FixedAt { at } => *at,
            Self::External { deadline, .. } => *deadline,
        }
    }

    pub fn source(&self) -> Option<&ResolutionSourceId> {
        match self {
            Self::External { source, .. } => Some(source),
            Self::FixedAt { .. } => None,
        }
    }
}

impl CanonicalTerms for ResolutionRule {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        match self {
            Self::FixedAt { at } => {
                encoder.write_label("fixed-at");
                encoder.write_i64(*at);
            }
            Self::External { source, deadline } => {
                encoder.write_label("external");
                encoder.write_label(source.as_str());
                encoder.write_i64(*deadline);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutcomeGroupSeed {
    pub question: smol_str::SmolStr,
    pub expiry: Expiry,
    pub topology: OutcomeTopology,
    pub settlement_asset: AssetId,
    pub resolution: ResolutionRule,
    pub fraction_rule: SettlementFractionRule,
}

impl OutcomeGroupSeed {
    pub fn validate(&self) -> Result<(), ModelError> {
        validate_question(&self.question)
    }

    pub fn seed_id(&self) -> OutcomeGroupId {
        OutcomeGroupId::of_terms(self)
    }
}

impl CanonicalTerms for OutcomeGroupSeed {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label("outcome-group-seed");
        encoder.write_label(&self.question);
        encoder.write_terms(&self.expiry);
        encoder.write_terms(&self.topology);
        encoder.write_terms(&self.settlement_asset);
        encoder.write_terms(&self.resolution);
        encoder.write_terms(&self.fraction_rule);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
struct OutcomeSideTerms {
    seed: OutcomeGroupId,
    label: smol_str::SmolStr,
}

impl CanonicalTerms for OutcomeSideTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label("outcome-side");
        encoder.write_terms(&self.seed);
        encoder.write_label(&self.label);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
struct OutcomeMemberTerms {
    seed: OutcomeGroupId,
    label: smol_str::SmolStr,
    sides: Vec<OutcomeSideId>,
}

impl CanonicalTerms for OutcomeMemberTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label("outcome-member");
        encoder.write_terms(&self.seed);
        encoder.write_label(&self.label);
        encoder.write_terms_slice(&self.sides);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
struct OutcomeEventTerms {
    source: ResolutionSourceId,
    native_event: IdentifierValue,
}

impl CanonicalTerms for OutcomeEventTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label("outcome-event");
        encoder.write_label(self.source.as_str());
        encoder.write_label(self.native_event.as_str());
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct OutcomeSide {
    #[serde(rename = "id")]
    pub id: OutcomeSideId,
    #[serde(rename = "label")]
    pub label: smol_str::SmolStr,
}

impl OutcomeSide {
    pub fn new(seed: &OutcomeGroupId, label: smol_str::SmolStr) -> Result<Self, ModelError> {
        if label.is_empty() {
            return Err(ModelError::EmptyIdentifier {
                field: "outcome_side.label",
            });
        }
        let terms = OutcomeSideTerms {
            seed: *seed,
            label: label.clone(),
        };
        Ok(Self {
            id: OutcomeSideId::of_terms(&terms),
            label,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct OutcomeMember {
    #[serde(rename = "id")]
    pub id: OutcomeMemberId,
    #[serde(rename = "label")]
    pub label: smol_str::SmolStr,
    #[serde(rename = "sides")]
    pub sides: Vec<OutcomeSideId>,
}

impl OutcomeMember {
    pub fn new(
        seed: &OutcomeGroupId,
        label: smol_str::SmolStr,
        sides: Vec<OutcomeSideId>,
    ) -> Result<Self, ModelError> {
        if label.is_empty() {
            return Err(ModelError::EmptyIdentifier {
                field: "outcome_member.label",
            });
        }
        if sides.is_empty() {
            return Err(ModelError::OutcomeGroupUnresolved {
                field: "member without tradable sides",
            });
        }
        let mut side_ids = sides;
        side_ids.sort();
        side_ids.dedup();
        let terms = OutcomeMemberTerms {
            seed: *seed,
            label: label.clone(),
            sides: side_ids.clone(),
        };
        Ok(Self {
            id: OutcomeMemberId::of_terms(&terms),
            label,
            sides: side_ids,
        })
    }
}

impl CanonicalTerms for OutcomeMember {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label(self.id.as_str().as_str());
        encoder.write_label(&self.label);
        encoder.write_terms_slice(&self.sides);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutcomeGroupTerms {
    pub question: smol_str::SmolStr,
    pub expiry: Expiry,
    pub topology: OutcomeTopology,
    pub settlement_asset: AssetId,
    pub resolution: ResolutionRule,
    pub fraction_rule: SettlementFractionRule,
    pub members: Vec<OutcomeMember>,
}

impl OutcomeGroupTerms {
    pub fn group_id(&self) -> OutcomeGroupId {
        OutcomeGroupId::of_terms(self)
    }

    fn validate_terms(&self) -> Result<(), ModelError> {
        let incomplete = |field: &'static str| ModelError::OutcomeGroupUnresolved { field };
        validate_question(&self.question)?;
        if !self.settlement_asset.is_settlement_asset() {
            return Err(ModelError::UnsupportedSettlementAsset {
                asset: self.settlement_asset.clone(),
                role: SettlementAssetRole::OutcomeGroup,
            });
        }
        if self.members.is_empty() {
            return Err(incomplete("group without members"));
        }
        let member_ids: Vec<OutcomeMemberId> =
            self.members.iter().map(|member| member.id).collect();
        require_sorted_and_unique(&member_ids, "outcome_group.members")?;
        let known: BTreeSet<&OutcomeSideId> = self
            .members
            .iter()
            .flat_map(|member| member.sides.iter())
            .collect();
        for member in &self.members {
            if member.sides.is_empty() {
                return Err(incomplete("member without sides"));
            }
            if member.sides.iter().any(|side| !known.contains(side)) {
                return Err(incomplete("member references an unknown side"));
            }
        }
        match self.topology {
            OutcomeTopology::Binary => {
                if self.members.len() != 1 {
                    return Err(incomplete("binary group must have exactly one member"));
                }
                if self.members[0].sides.len() != 2 {
                    return Err(incomplete("binary member must have exactly two sides"));
                }
            }
            OutcomeTopology::MutuallyExclusive(_) => {
                if self.members.len() < 2 {
                    return Err(incomplete(
                        "mutually exclusive group must have at least two members",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl CanonicalTerms for OutcomeGroupTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label("outcome-group");
        encoder.write_label(&self.question);
        encoder.write_terms(&self.expiry);
        encoder.write_terms(&self.topology);
        encoder.write_terms(&self.settlement_asset);
        encoder.write_terms(&self.resolution);
        encoder.write_terms(&self.fraction_rule);
        encoder.write_terms_slice(&self.members);
    }
}

pub fn validate_question(question: &smol_str::SmolStr) -> Result<(), ModelError> {
    if question.is_empty() {
        return Err(ModelError::OutcomeGroupUnresolved {
            field: "empty question",
        });
    }
    if question.chars().count() > MAX_OUTCOME_QUESTION_SCALARS {
        return Err(ModelError::OutcomeQuestionTooLong {
            maximum: MAX_OUTCOME_QUESTION_SCALARS,
        });
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutcomeGroup {
    pub id: OutcomeGroupId,
    pub question: smol_str::SmolStr,
    pub expiry: Expiry,
    pub topology: OutcomeTopology,
    pub settlement_asset: AssetId,
    pub resolution: ResolutionRule,
    pub fraction_rule: SettlementFractionRule,
    pub sides: Vec<OutcomeSide>,
    pub members: Vec<OutcomeMember>,
}

impl OutcomeGroup {
    pub fn new(
        seed: OutcomeGroupSeed,
        sides: Vec<OutcomeSide>,
        mut members: Vec<OutcomeMember>,
    ) -> Result<Self, ModelError> {
        seed.validate()?;
        members.sort_by_key(|left| left.id);
        let terms = OutcomeGroupTerms {
            question: seed.question,
            expiry: seed.expiry,
            topology: seed.topology,
            settlement_asset: seed.settlement_asset,
            resolution: seed.resolution,
            fraction_rule: seed.fraction_rule,
            members,
        };
        terms.validate_terms()?;
        terms.expiry.validate()?;
        terms.resolution.validate(&terms.expiry)?;
        let mut side_ids: Vec<OutcomeSideId> = sides.iter().map(|side| side.id).collect();
        side_ids.sort();
        require_sorted_and_unique(&side_ids, "outcome_group.sides")?;
        let declared: BTreeSet<&OutcomeSideId> = side_ids.iter().collect();
        for member in &terms.members {
            if member.sides.iter().any(|side| !declared.contains(side)) {
                return Err(ModelError::OutcomeGroupUnresolved {
                    field: "member side is not declared by the group",
                });
            }
        }
        let id = terms.group_id();
        Ok(Self {
            id,
            question: terms.question,
            expiry: terms.expiry,
            topology: terms.topology,
            settlement_asset: terms.settlement_asset,
            resolution: terms.resolution,
            fraction_rule: terms.fraction_rule,
            sides,
            members: terms.members,
        })
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        let terms = OutcomeGroupTerms {
            question: self.question.clone(),
            expiry: self.expiry,
            topology: self.topology,
            settlement_asset: self.settlement_asset.clone(),
            resolution: self.resolution.clone(),
            fraction_rule: self.fraction_rule,
            members: self.members.clone(),
        };
        terms.validate_terms()?;
        self.expiry.validate()?;
        self.resolution.validate(&self.expiry)?;
        if terms.group_id() != self.id {
            return Err(ModelError::OutcomeGroupIncomplete {
                group: self.id,
                field: "group id does not match the canonical terms",
            });
        }
        Ok(())
    }

    pub fn member(&self, member: &OutcomeMemberId) -> Option<&OutcomeMember> {
        self.members
            .iter()
            .find(|candidate| &candidate.id == member)
    }

    pub fn side(&self, side: &OutcomeSideId) -> Option<&OutcomeSide> {
        self.sides.iter().find(|candidate| &candidate.id == side)
    }

    pub fn tradable_legs(&self) -> Vec<(&OutcomeMemberId, &OutcomeSideId)> {
        self.members
            .iter()
            .flat_map(|member| member.sides.iter().map(|side| (&member.id, side)))
            .collect()
    }
}

impl<'de> Deserialize<'de> for OutcomeGroup {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = OutcomeGroupWire::deserialize(deserializer)?;
        Self::try_from(wire).map_err(de::Error::custom)
    }
}

#[derive(Deserialize)]
struct OutcomeGroupWire {
    #[serde(rename = "question")]
    question: smol_str::SmolStr,
    #[serde(rename = "expiry")]
    expiry: ExpiryWire,
    #[serde(rename = "topology")]
    topology: OutcomeTopology,
    #[serde(rename = "settlement_asset")]
    settlement_asset: AssetId,
    #[serde(rename = "resolution")]
    resolution: ResolutionRule,
    #[serde(rename = "fraction_rule")]
    fraction_rule: SettlementFractionRule,
    #[serde(rename = "sides")]
    sides: Vec<OutcomeSideWire>,
    #[serde(rename = "members")]
    members: Vec<OutcomeMemberWire>,
}

#[derive(Serialize)]
struct OutcomeGroupDto<'a> {
    #[serde(rename = "question")]
    question: &'a smol_str::SmolStr,
    #[serde(rename = "expiry")]
    expiry: &'a Expiry,
    #[serde(rename = "topology")]
    topology: OutcomeTopology,
    #[serde(rename = "settlement_asset")]
    settlement_asset: &'a AssetId,
    #[serde(rename = "resolution")]
    resolution: &'a ResolutionRule,
    #[serde(rename = "fraction_rule")]
    fraction_rule: SettlementFractionRule,
    #[serde(rename = "sides")]
    sides: Vec<OutcomeSideWireDto<'a>>,
    #[serde(rename = "members")]
    members: Vec<OutcomeMemberWireDto<'a>>,
}

#[derive(Serialize)]
struct OutcomeSideWireDto<'a> {
    #[serde(rename = "label")]
    label: &'a smol_str::SmolStr,
}

#[derive(Serialize)]
struct OutcomeMemberWireDto<'a> {
    #[serde(rename = "label")]
    label: &'a smol_str::SmolStr,
    #[serde(rename = "sides")]
    sides: Vec<&'a smol_str::SmolStr>,
}

impl Serialize for OutcomeGroup {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let labels: AHashMap<&OutcomeSideId, &smol_str::SmolStr> = self
            .sides
            .iter()
            .map(|side| (&side.id, &side.label))
            .collect();
        let members = self
            .members
            .iter()
            .map(|member| OutcomeMemberWireDto {
                label: &member.label,
                sides: member
                    .sides
                    .iter()
                    .filter_map(|side| labels.get(side).copied())
                    .collect(),
            })
            .collect();
        OutcomeGroupDto {
            question: &self.question,
            expiry: &self.expiry,
            topology: self.topology,
            settlement_asset: &self.settlement_asset,
            resolution: &self.resolution,
            fraction_rule: self.fraction_rule,
            sides: self
                .sides
                .iter()
                .map(|side| OutcomeSideWireDto { label: &side.label })
                .collect(),
            members,
        }
        .serialize(serializer)
    }
}

#[derive(Deserialize)]
struct OutcomeSideWire {
    #[serde(rename = "label")]
    label: smol_str::SmolStr,
}

#[derive(Deserialize)]
struct OutcomeMemberWire {
    #[serde(rename = "label")]
    label: smol_str::SmolStr,
    #[serde(rename = "sides")]
    sides: Vec<smol_str::SmolStr>,
}

impl TryFrom<OutcomeGroupWire> for OutcomeGroup {
    type Error = ModelError;

    fn try_from(wire: OutcomeGroupWire) -> Result<Self, Self::Error> {
        let seed = OutcomeGroupSeed {
            question: wire.question,
            expiry: Expiry::try_from(wire.expiry)?,
            topology: wire.topology,
            settlement_asset: wire.settlement_asset,
            resolution: wire.resolution,
            fraction_rule: wire.fraction_rule,
        };
        let seed_id = seed.seed_id();
        let sides: Vec<OutcomeSide> = wire
            .sides
            .into_iter()
            .map(|side| OutcomeSide::new(&seed_id, side.label))
            .collect::<Result<_, _>>()?;
        let mut by_label: AHashMap<smol_str::SmolStr, OutcomeSideId> =
            AHashMap::with_capacity(sides.len());
        for side in &sides {
            by_label.insert(side.label.clone(), side.id);
        }
        let members = wire
            .members
            .into_iter()
            .map(|member| {
                let member_sides = member
                    .sides
                    .iter()
                    .map(|label| {
                        by_label
                            .get(label)
                            .cloned()
                            .ok_or(ModelError::OutcomeGroupUnresolved {
                                field: "member references an undeclared side label",
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                OutcomeMember::new(&seed_id, member.label, member_sides)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(seed, sides, members)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum OutcomeGroupAdjustment {
    #[serde(rename = "question_change")]
    QuestionChange,
    #[serde(rename = "resolution_change")]
    ResolutionChange,
    #[serde(rename = "topology_change")]
    TopologyChange,
    #[serde(rename = "settlement_asset_change")]
    SettlementAssetChange,
    #[serde(rename = "settlement_rule_change")]
    SettlementRuleChange,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OutcomeGroupLineageEdge {
    #[serde(rename = "predecessor")]
    pub predecessor: OutcomeGroupId,
    #[serde(rename = "successor")]
    pub successor: OutcomeGroupId,
    #[serde(rename = "reason")]
    pub reason: OutcomeGroupAdjustment,
    #[serde(rename = "effective_at")]
    pub effective_at: UnixMillis,
}

impl OutcomeGroupLineageEdge {
    pub fn new(
        predecessor: OutcomeGroupId,
        successor: OutcomeGroupId,
        reason: OutcomeGroupAdjustment,
        effective_at: UnixMillis,
    ) -> Result<Self, ModelError> {
        require_non_negative_time(effective_at, "effective_at")?;
        if predecessor == successor {
            return Err(ModelError::OutcomeGroupLineageInvalid {
                field: "successor equals predecessor",
            });
        }
        Ok(Self {
            predecessor,
            successor,
            reason,
            effective_at,
        })
    }
}

impl LineageRelation for OutcomeGroupLineageEdge {
    fn predecessor(&self) -> IdentityText {
        self.predecessor.as_str()
    }

    fn successor(&self) -> IdentityText {
        self.successor.as_str()
    }

    fn effective_at(&self) -> UnixMillis {
        self.effective_at
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum OutcomeRiskPoolAdjustment {
    #[serde(rename = "group_membership_change")]
    GroupMembershipChange,
    #[serde(rename = "conversion_rule_change")]
    ConversionRuleChange,
    #[serde(rename = "settlement_asset_change")]
    SettlementAssetChange,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OutcomeRiskPoolLineageEdge {
    #[serde(rename = "predecessor")]
    pub predecessor: OutcomeRiskPoolId,
    #[serde(rename = "successor")]
    pub successor: OutcomeRiskPoolId,
    #[serde(rename = "reason")]
    pub reason: OutcomeRiskPoolAdjustment,
    #[serde(rename = "effective_at")]
    pub effective_at: UnixMillis,
}

impl OutcomeRiskPoolLineageEdge {
    pub fn new(
        predecessor: OutcomeRiskPoolId,
        successor: OutcomeRiskPoolId,
        reason: OutcomeRiskPoolAdjustment,
        effective_at: UnixMillis,
    ) -> Result<Self, ModelError> {
        require_non_negative_time(effective_at, "effective_at")?;
        if predecessor == successor {
            return Err(ModelError::OutcomeRiskPoolLineageInvalid {
                field: "successor equals predecessor",
            });
        }
        Ok(Self {
            predecessor,
            successor,
            reason,
            effective_at,
        })
    }
}

impl LineageRelation for OutcomeRiskPoolLineageEdge {
    fn predecessor(&self) -> IdentityText {
        self.predecessor.as_str()
    }

    fn successor(&self) -> IdentityText {
        self.successor.as_str()
    }

    fn effective_at(&self) -> UnixMillis {
        self.effective_at
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OutcomeLegRef {
    #[serde(rename = "group")]
    pub group: OutcomeGroupId,
    #[serde(rename = "member")]
    pub member: OutcomeMemberId,
    #[serde(rename = "side")]
    pub side: OutcomeSideId,
}

impl CanonicalTerms for OutcomeLegRef {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_terms(&self.group);
        encoder.write_terms(&self.member);
        encoder.write_terms(&self.side);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OutcomeConversionOutput {
    #[serde(rename = "leg")]
    pub leg: OutcomeLegRef,
    #[serde(rename = "factor")]
    pub factor: PositiveRatio,
}

impl CanonicalTerms for OutcomeConversionOutput {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_terms(&self.leg);
        encoder.write_terms(&self.factor);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OutcomeConversion {
    #[serde(rename = "input")]
    pub input: OutcomeLegRef,
    #[serde(rename = "outputs")]
    pub outputs: Vec<OutcomeConversionOutput>,
}

impl OutcomeConversion {
    pub fn normalized(mut self) -> Result<Self, ModelError> {
        if self.outputs.is_empty() {
            return Err(ModelError::OutcomeRiskPoolUnresolved {
                field: "conversion without outputs",
            });
        }
        self.outputs.sort();
        let mut deduplicated: Vec<OutcomeConversionOutput> = Vec::with_capacity(self.outputs.len());
        for output in self.outputs {
            if deduplicated
                .last()
                .is_some_and(|previous| previous.leg == output.leg)
            {
                return Err(ModelError::DuplicateCollectionEntry {
                    field: "outcome_conversion.outputs",
                });
            }
            deduplicated.push(output);
        }
        self.outputs = deduplicated;
        Ok(self)
    }
}

impl CanonicalTerms for OutcomeConversion {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_terms(&self.input);
        encoder.write_terms_slice(&self.outputs);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutcomeRiskPoolTerms {
    pub settlement_asset: AssetId,
    pub groups: Vec<OutcomeGroupId>,
    pub conversions: Vec<OutcomeConversion>,
}

impl OutcomeRiskPoolTerms {
    pub fn risk_pool_id(&self) -> OutcomeRiskPoolId {
        OutcomeRiskPoolId::of_terms(self)
    }
}

impl CanonicalTerms for OutcomeRiskPoolTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label("outcome-risk-pool");
        encoder.write_terms(&self.settlement_asset);
        encoder.write_terms_slice(&self.groups);
        encoder.write_terms_slice(&self.conversions);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OutcomeRiskPool {
    #[serde(rename = "id")]
    pub id: OutcomeRiskPoolId,
    #[serde(rename = "settlement_asset")]
    pub settlement_asset: AssetId,
    #[serde(rename = "groups")]
    pub groups: Vec<OutcomeGroupId>,
    #[serde(rename = "conversions")]
    pub conversions: Vec<OutcomeConversion>,
}

impl OutcomeRiskPool {
    pub fn new(
        settlement_asset: AssetId,
        mut groups: Vec<OutcomeGroupId>,
        conversions: Vec<OutcomeConversion>,
    ) -> Result<Self, ModelError> {
        groups.sort();
        let mut normalized = Vec::with_capacity(conversions.len());
        for conversion in conversions {
            normalized.push(conversion.normalized()?);
        }
        normalized.sort();
        let terms = OutcomeRiskPoolTerms {
            settlement_asset,
            groups,
            conversions: normalized,
        };
        let pool = Self {
            id: terms.risk_pool_id(),
            settlement_asset: terms.settlement_asset,
            groups: terms.groups,
            conversions: terms.conversions,
        };
        pool.validate()?;
        Ok(pool)
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        let incomplete = |field: &'static str| ModelError::OutcomeRiskPoolIncomplete {
            pool: self.id,
            field,
        };
        if self.groups.len() < 2 {
            return Err(incomplete("risk pool must relate at least two groups"));
        }
        require_sorted_and_unique(&self.groups, "outcome_risk_pool.groups")?;
        if !self.settlement_asset.is_settlement_asset() {
            return Err(ModelError::UnsupportedSettlementAsset {
                asset: self.settlement_asset.clone(),
                role: SettlementAssetRole::OutcomeRiskPool,
            });
        }
        let members: BTreeSet<&OutcomeGroupId> = self.groups.iter().collect();
        let mut adjacency: AHashMap<&OutcomeGroupId, Vec<&OutcomeGroupId>> =
            AHashMap::with_capacity(self.groups.len());
        for conversion in &self.conversions {
            if !members.contains(&conversion.input.group) {
                return Err(ModelError::OutcomeConversionUnresolved {
                    pool: self.id,
                    field: "conversion input group",
                });
            }
            for output in &conversion.outputs {
                if !members.contains(&output.leg.group) {
                    return Err(ModelError::OutcomeConversionUnresolved {
                        pool: self.id,
                        field: "conversion output group",
                    });
                }
                adjacency
                    .entry(&conversion.input.group)
                    .or_default()
                    .push(&output.leg.group);
            }
        }
        for group in &self.groups {
            let mut stack = vec![group];
            let mut visited = 0_usize;
            while let Some(current) = stack.pop() {
                visited += 1;
                if visited > self.groups.len() * self.groups.len() + 1 {
                    return Err(ModelError::OutcomeConversionCycle);
                }
                if let Some(next) = adjacency.get(current) {
                    if next.contains(&group) {
                        return Err(ModelError::OutcomeConversionCycle);
                    }
                    stack.extend(next.iter().copied());
                }
            }
        }
        Ok(())
    }

    pub fn contains(&self, group: &OutcomeGroupId) -> bool {
        self.groups.iter().any(|candidate| candidate == group)
    }
}

impl<'de> Deserialize<'de> for OutcomeRiskPool {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = OutcomeRiskPoolWire::deserialize(deserializer)?;
        Self::new(wire.settlement_asset, wire.groups, wire.conversions).map_err(de::Error::custom)
    }
}

#[derive(Deserialize)]
struct OutcomeRiskPoolWire {
    #[serde(rename = "settlement_asset")]
    settlement_asset: AssetId,
    #[serde(rename = "groups")]
    groups: Vec<OutcomeGroupId>,
    #[serde(rename = "conversions")]
    conversions: Vec<OutcomeConversion>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OutcomeEvent {
    #[serde(rename = "id")]
    pub id: OutcomeEventId,
    #[serde(rename = "source")]
    pub source: ResolutionSourceId,
    #[serde(rename = "native_event")]
    pub native_event: IdentifierValue,
    #[serde(rename = "groups")]
    pub groups: Vec<OutcomeGroupId>,
    #[serde(rename = "risk_pools")]
    pub risk_pools: Vec<OutcomeRiskPoolId>,
}

impl OutcomeEvent {
    pub fn new(
        source: ResolutionSourceId,
        native_event: IdentifierValue,
        mut groups: Vec<OutcomeGroupId>,
        mut risk_pools: Vec<OutcomeRiskPoolId>,
    ) -> Result<Self, ModelError> {
        groups.sort();
        risk_pools.sort();
        require_sorted_and_unique(&groups, "outcome_event.groups")?;
        require_sorted_and_unique(&risk_pools, "outcome_event.risk_pools")?;
        let terms = OutcomeEventTerms {
            source: source.clone(),
            native_event: native_event.clone(),
        };
        Ok(Self {
            id: OutcomeEventId::of_terms(&terms),
            source,
            native_event,
            groups,
            risk_pools,
        })
    }
}

impl<'de> Deserialize<'de> for OutcomeEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = OutcomeEventWire::deserialize(deserializer)?;
        Self::new(wire.source, wire.native_event, wire.groups, wire.risk_pools)
            .map_err(de::Error::custom)
    }
}

#[derive(Deserialize)]
struct OutcomeEventWire {
    #[serde(rename = "source")]
    source: ResolutionSourceId,
    #[serde(rename = "native_event")]
    native_event: IdentifierValue,
    #[serde(rename = "groups")]
    groups: Vec<OutcomeGroupId>,
    #[serde(rename = "risk_pools")]
    risk_pools: Vec<OutcomeRiskPoolId>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OutcomeSettlementLeg {
    #[serde(rename = "member")]
    pub member: OutcomeMemberId,
    #[serde(rename = "side")]
    pub side: OutcomeSideId,
    #[serde(rename = "settle_fraction")]
    pub settle_fraction: Fraction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OutcomeSettlement {
    #[serde(rename = "group")]
    pub group: OutcomeGroupId,
    #[serde(rename = "legs")]
    pub legs: Vec<OutcomeSettlementLeg>,
    #[serde(rename = "settled_at")]
    pub settled_at: UnixMillis,
    #[serde(rename = "source")]
    pub source: Option<ResolutionSourceId>,
}

impl OutcomeSettlement {
    pub fn validate_against(&self, group: &OutcomeGroup) -> Result<(), ModelError> {
        require_non_negative_time(self.settled_at, "settled_at")?;
        if self.group != group.id {
            return Err(ModelError::OutcomeSettlementIncomplete { group: group.id });
        }
        for (member, side) in group.tradable_legs() {
            let Some(leg) = self
                .legs
                .iter()
                .find(|leg| &leg.member == member && &leg.side == side)
            else {
                return Err(ModelError::OutcomeSettlementIncomplete { group: group.id });
            };
            group
                .fraction_rule
                .validate_fraction(&group.id, leg.settle_fraction)?;
        }
        for leg in &self.legs {
            let member = group
                .member(&leg.member)
                .ok_or(ModelError::OutcomeSettlementIncomplete { group: group.id })?;
            if !member.sides.contains(&leg.side) {
                return Err(ModelError::OutcomeSettlementIncomplete { group: group.id });
            }
        }
        self.validate_topology(group)
    }

    fn validate_topology(&self, group: &OutcomeGroup) -> Result<(), ModelError> {
        let fraction_of = |member: &OutcomeMemberId, side: &OutcomeSideId| {
            self.legs
                .iter()
                .find(|leg| &leg.member == member && &leg.side == side)
                .map(|leg| leg.settle_fraction)
        };
        for member in &group.members {
            if member.sides.len() != 2 {
                continue;
            }
            let first = fraction_of(&member.id, &member.sides[0]);
            let second = fraction_of(&member.id, &member.sides[1]);
            if let (Some(first), Some(second)) = (first, second)
                && first.complement() != second
            {
                return Err(ModelError::OutcomeSettlementFractionInvalid {
                    group: group.id,
                    fraction_kind: "complementary",
                });
            }
        }
        if let OutcomeTopology::MutuallyExclusive(cardinality) = group.topology {
            let selected = group
                .members
                .iter()
                .filter(|member| {
                    member
                        .sides
                        .iter()
                        .any(|side| fraction_of(&member.id, side) == Some(Fraction::ONE))
                })
                .count();
            match cardinality {
                SelectionCardinality::ExactlyOne if selected != 1 => {
                    return Err(ModelError::OutcomeSettlementFractionInvalid {
                        group: group.id,
                        fraction_kind: "exactly-one",
                    });
                }
                SelectionCardinality::AtMostOne if selected > 1 => {
                    return Err(ModelError::OutcomeSettlementFractionInvalid {
                        group: group.id,
                        fraction_kind: "at-most-one",
                    });
                }
                _ => {}
            }
        }
        Ok(())
    }
}

pub fn validate_risk_pool_membership(pools: &[OutcomeRiskPool]) -> Result<(), ModelError> {
    let mut owner: AHashMap<&OutcomeGroupId, &OutcomeRiskPoolId> = AHashMap::new();
    for pool in pools {
        for group in &pool.groups {
            if owner.insert(group, &pool.id).is_some() {
                return Err(ModelError::OutcomeRiskPoolIncomplete {
                    pool: pool.id,
                    field: "group already belongs to another risk pool",
                });
            }
        }
    }
    Ok(())
}

pub fn validate_pool_successors(
    group_edges: &[OutcomeGroupLineageEdge],
    pool_edges: &[OutcomeRiskPoolLineageEdge],
    pools: &[OutcomeRiskPool],
) -> Result<(), ModelError> {
    let mut has_successor: AHashSet<OutcomeGroupId> = AHashSet::with_capacity(group_edges.len());
    for edge in group_edges {
        has_successor.insert(edge.predecessor);
    }
    let predecessor_of = |pool: &OutcomeRiskPoolId| {
        pool_edges
            .iter()
            .find(|edge| &edge.predecessor == pool)
            .map(|edge| &edge.successor)
    };
    for pool in pools {
        if !pool
            .groups
            .iter()
            .any(|group| has_successor.contains(group))
        {
            continue;
        }
        if predecessor_of(&pool.id).is_none() {
            return Err(ModelError::OutcomeRiskPoolLineageInvalid {
                field: "risk pool member group has a successor without a pool successor",
            });
        }
    }
    Ok(())
}

pub fn validate_outcome_group_lineage(edges: &[OutcomeGroupLineageEdge]) -> Result<(), ModelError> {
    validate_lineage(edges, |field| ModelError::OutcomeGroupLineageInvalid {
        field,
    })
}

pub fn validate_outcome_risk_pool_lineage(
    edges: &[OutcomeRiskPoolLineageEdge],
) -> Result<(), ModelError> {
    validate_lineage(edges, |field| ModelError::OutcomeRiskPoolLineageInvalid {
        field,
    })
}

pub fn leg_label(leg: &OutcomeLegRef) -> smol_str::SmolStr {
    smol_str::format_smolstr!("{}/{}/{}", leg.group, leg.member, leg.side)
}

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;

    use crate::{
        asset::{
            AssetId, CryptoTokenId, CryptoTokenOrigin, SecurityForm, SecurityId, SecurityIssuerId,
            ShareClassId, ShareClassTerms, TokenReference,
        },
        decimal::{Fraction, PositiveRatio, Ratio, UnixMillis},
        error::ModelError,
        identifier::{IdentifierNamespace, IdentifierValue, ResolutionSourceId, TokenIssuerId},
        outcome::{
            MAX_OUTCOME_QUESTION_SCALARS, OutcomeConversion, OutcomeConversionOutput, OutcomeEvent,
            OutcomeGroup, OutcomeGroupAdjustment, OutcomeGroupLineageEdge, OutcomeGroupSeed,
            OutcomeLegRef, OutcomeMember, OutcomeRiskPool, OutcomeRiskPoolAdjustment,
            OutcomeRiskPoolId, OutcomeRiskPoolLineageEdge, OutcomeSettlement, OutcomeSettlementLeg,
            OutcomeSide, OutcomeTopology, ResolutionRule, SelectionCardinality,
            SettlementFractionRule, validate_outcome_group_lineage, validate_pool_successors,
            validate_question, validate_risk_pool_membership,
        },
        settlement::{Expiry, LineageRelation},
    };

    fn usdc() -> AssetId {
        AssetId::CryptoToken(
            CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
                issuer: TokenIssuerId::new("hyperliquid").expect("issuer should be valid"),
                namespace: IdentifierNamespace::new("hyperliquid")
                    .expect("namespace should be valid"),
                token: TokenReference::Native {
                    code: IdentifierValue::new("USDC").expect("code should be valid"),
                },
            })
            .expect("token should be valid"),
        )
    }

    fn expiry() -> Expiry {
        Expiry::new(2_000, Some(1_500), Some(2_500)).expect("expiry should be valid")
    }

    fn binary_group(question: &str) -> OutcomeGroup {
        let seed = OutcomeGroupSeed {
            question: question.into(),
            expiry: expiry(),
            topology: OutcomeTopology::Binary,
            settlement_asset: usdc(),
            resolution: ResolutionRule::External {
                source: ResolutionSourceId::new("uma").expect("source should be valid"),
                deadline: 1_900,
            },
            fraction_rule: SettlementFractionRule::AnyFraction,
        };
        let seed_id = seed.seed_id();
        let yes = OutcomeSide::new(&seed_id, "Yes".into()).expect("side should be valid");
        let no = OutcomeSide::new(&seed_id, "No".into()).expect("side should be valid");
        let member = OutcomeMember::new(&seed_id, "winner".into(), vec![yes.id, no.id])
            .expect("member should be valid");
        OutcomeGroup::new(seed, vec![yes, no], vec![member]).expect("group should be valid")
    }

    fn exclusive_group(question: &str, labels: &[&str]) -> OutcomeGroup {
        exclusive_group_with(question, labels, SelectionCardinality::ExactlyOne)
    }

    fn exclusive_group_with(
        question: &str,
        labels: &[&str],
        cardinality: SelectionCardinality,
    ) -> OutcomeGroup {
        let seed = OutcomeGroupSeed {
            question: question.into(),
            expiry: expiry(),
            topology: OutcomeTopology::MutuallyExclusive(cardinality),
            settlement_asset: usdc(),
            resolution: ResolutionRule::FixedAt { at: 1_900 },
            fraction_rule: SettlementFractionRule::Binary,
        };
        let seed_id = seed.seed_id();
        let sides: Vec<OutcomeSide> = labels
            .iter()
            .map(|label| OutcomeSide::new(&seed_id, (*label).into()).expect("side should be valid"))
            .collect();
        let members: Vec<OutcomeMember> = sides
            .iter()
            .enumerate()
            .map(|(index, side)| {
                OutcomeMember::new(&seed_id, format!("member-{index}").into(), vec![side.id])
                    .expect("member should be valid")
            })
            .collect();
        OutcomeGroup::new(seed, sides, members).expect("group should be valid")
    }

    fn conversion(
        input: &OutcomeGroup,
        input_side: usize,
        output: &OutcomeGroup,
        output_side: usize,
    ) -> OutcomeConversion {
        OutcomeConversion {
            input: OutcomeLegRef {
                group: input.id,
                member: input.members[0].id,
                side: input.members[0].sides[input_side],
            },
            outputs: vec![OutcomeConversionOutput {
                leg: OutcomeLegRef {
                    group: output.id,
                    member: output.members[0].id,
                    side: output.members[0].sides[output_side],
                },
                factor: PositiveRatio::new(Ratio::from(1_u8)).expect("factor should be positive"),
            }],
        }
    }

    fn settlement(
        group: &OutcomeGroup,
        fractions: &[Fraction],
        settled_at: UnixMillis,
    ) -> OutcomeSettlement {
        let legs = group
            .members
            .iter()
            .flat_map(|member| {
                member
                    .sides
                    .iter()
                    .enumerate()
                    .map(|(index, side)| OutcomeSettlementLeg {
                        member: member.id,
                        side: *side,
                        settle_fraction: fractions[index],
                    })
            })
            .collect();
        OutcomeSettlement {
            group: group.id,
            legs,
            settled_at,
            source: None,
        }
    }

    #[test]
    fn outcome_group_identity_should_be_a_pinned_digest() {
        assert_eq!(
            binary_group("Will it rain?").id.as_str(),
            "v1/50352fa72d17b14e5725e5ada07d53595e2f26c444245b5643d57e37e74ff19d",
            "the full binary group terms derive a pinned group identity"
        );
    }

    #[test]
    fn binary_group_should_require_exactly_one_member_with_two_sides() {
        let group = binary_group("Will it rain?");
        let seed = OutcomeGroupSeed {
            question: "Will it rain?".into(),
            expiry: expiry(),
            topology: OutcomeTopology::Binary,
            settlement_asset: usdc(),
            resolution: ResolutionRule::FixedAt { at: 1_900 },
            fraction_rule: SettlementFractionRule::Binary,
        };
        let seed_id = seed.seed_id();
        let single_side = OutcomeSide::new(&seed_id, "Yes".into()).expect("side should be valid");
        let member = OutcomeMember::new(&seed_id, "winner".into(), vec![single_side.id])
            .expect("member should be valid");

        assert!(matches!(
            OutcomeGroup::new(seed, vec![single_side], vec![member]),
            Err(ModelError::OutcomeGroupUnresolved { .. })
        ));
        assert_eq!(group.members.len(), 1);
        assert_eq!(group.members[0].sides.len(), 2);
    }

    #[test]
    fn outcome_group_should_reject_a_non_settlement_asset() {
        let security = AssetId::Security(SecurityId::new(
            SecurityIssuerId::new("cik/320193").expect("issuer should be valid"),
            ShareClassId::new(&ShareClassTerms {
                issuer: SecurityIssuerId::new("cik/320193").expect("issuer should be valid"),
                designation: "common".into(),
                form: SecurityForm::CommonStock,
                par_value: None,
                adr_ratio: None,
            })
            .expect("share class should be valid"),
            SecurityForm::CommonStock,
        ));
        let seed = OutcomeGroupSeed {
            question: "question".into(),
            expiry: expiry(),
            topology: OutcomeTopology::Binary,
            settlement_asset: security,
            resolution: ResolutionRule::FixedAt { at: 1_900 },
            fraction_rule: SettlementFractionRule::Binary,
        };
        let seed_id = seed.seed_id();
        let yes = OutcomeSide::new(&seed_id, "Yes".into()).expect("side should be valid");
        let no = OutcomeSide::new(&seed_id, "No".into()).expect("side should be valid");
        let member = OutcomeMember::new(&seed_id, "winner".into(), vec![yes.id, no.id])
            .expect("member should be valid");

        assert!(matches!(
            OutcomeGroup::new(seed, vec![yes, no], vec![member]),
            Err(ModelError::UnsupportedSettlementAsset { .. })
        ));
    }

    #[test]
    fn mutually_exclusive_group_should_require_two_members() {
        assert_eq!(exclusive_group("Who wins?", &["a", "b"]).members.len(), 2);

        let seed = OutcomeGroupSeed {
            question: "Who wins?".into(),
            expiry: expiry(),
            topology: OutcomeTopology::MutuallyExclusive(SelectionCardinality::ExactlyOne),
            settlement_asset: usdc(),
            resolution: ResolutionRule::FixedAt { at: 1_900 },
            fraction_rule: SettlementFractionRule::Binary,
        };
        let seed_id = seed.seed_id();
        let side = OutcomeSide::new(&seed_id, "a".into()).expect("side should be valid");
        let member = OutcomeMember::new(&seed_id, "member-0".into(), vec![side.id])
            .expect("member should be valid");
        assert!(matches!(
            OutcomeGroup::new(seed, vec![side], vec![member]),
            Err(ModelError::OutcomeGroupUnresolved { .. })
        ));
    }

    #[test]
    fn mutually_exclusive_cardinality_should_be_distinguishable() {
        assert_ne!(
            OutcomeTopology::MutuallyExclusive(SelectionCardinality::AtMostOne),
            OutcomeTopology::MutuallyExclusive(SelectionCardinality::ExactlyOne)
        );
        assert_eq!(
            OutcomeTopology::MutuallyExclusive(SelectionCardinality::ExactlyOne).cardinality(),
            Some(SelectionCardinality::ExactlyOne)
        );
        assert_eq!(OutcomeTopology::Binary.cardinality(), None);
    }

    #[test]
    fn question_text_should_change_the_group_identity() {
        let first = binary_group("Will it rain?");
        let second = binary_group("Will it rain tomorrow?");

        assert_ne!(first.id, second.id);
        assert_ne!(first.members[0].id, second.members[0].id);
    }

    #[test]
    fn side_label_should_change_the_member_identity() {
        let first = binary_group("Will it rain?");
        let seed_id = first.id;
        let renamed =
            OutcomeSide::new(&seed_id, "Yes indeed".into()).expect("side should be valid");
        let member = OutcomeMember::new(
            &seed_id,
            "winner".into(),
            vec![renamed.id, first.members[0].sides[1]],
        )
        .expect("member should be valid");

        assert_ne!(member.id, first.members[0].id);
    }

    #[test]
    fn question_should_be_limited_by_scalar_value_count() {
        let question = |scalars: usize| SmolStr::new("測".repeat(scalars));

        assert!(
            question(MAX_OUTCOME_QUESTION_SCALARS).len() > MAX_OUTCOME_QUESTION_SCALARS,
            "the fixture must actually use multi-byte scalar values"
        );
        assert_eq!(
            validate_question(&question(MAX_OUTCOME_QUESTION_SCALARS)),
            Ok(())
        );
        assert_eq!(
            validate_question(&question(MAX_OUTCOME_QUESTION_SCALARS + 1)),
            Err(ModelError::OutcomeQuestionTooLong {
                maximum: MAX_OUTCOME_QUESTION_SCALARS
            })
        );
        assert_eq!(
            validate_question(&SmolStr::new_static("")),
            Err(ModelError::OutcomeGroupUnresolved {
                field: "empty question"
            })
        );
    }

    #[test]
    fn group_should_reject_an_overlong_question_at_construction() {
        let seed = OutcomeGroupSeed {
            question: SmolStr::new("x".repeat(MAX_OUTCOME_QUESTION_SCALARS + 1)),
            expiry: expiry(),
            topology: OutcomeTopology::Binary,
            settlement_asset: usdc(),
            resolution: ResolutionRule::FixedAt { at: 1_900 },
            fraction_rule: SettlementFractionRule::Binary,
        };
        let seed_id = seed.seed_id();
        let yes = OutcomeSide::new(&seed_id, "Yes".into()).expect("side should be valid");
        let member = OutcomeMember::new(&seed_id, "main".into(), vec![yes.id])
            .expect("member should be valid");

        assert_eq!(
            seed.validate(),
            Err(ModelError::OutcomeQuestionTooLong {
                maximum: MAX_OUTCOME_QUESTION_SCALARS
            })
        );
        assert_eq!(
            OutcomeGroup::new(seed, vec![yes], vec![member]),
            Err(ModelError::OutcomeQuestionTooLong {
                maximum: MAX_OUTCOME_QUESTION_SCALARS
            }),
            "the group constructor shares the same limit"
        );
    }

    #[test]
    fn group_should_round_trip_through_its_wire_shape() {
        let group = binary_group("Will it rain?");
        let encoded = serde_json::to_string(&group).expect("group should serialize");
        let decoded = serde_json::from_str::<OutcomeGroup>(&encoded).expect("group should decode");

        assert_eq!(decoded, group);
        assert_eq!(group.validate(), Ok(()));
        assert_eq!(decoded.id, group.id);
        assert_eq!(decoded.members, group.members);
        assert_eq!(decoded.sides, group.sides);
    }

    #[test]
    fn group_lineage_should_reject_a_cycle() {
        let first = binary_group("Will it rain?");
        let second = binary_group("Will it rain tomorrow?");
        let edges = vec![
            OutcomeGroupLineageEdge::new(
                first.id,
                second.id,
                OutcomeGroupAdjustment::QuestionChange,
                0,
            )
            .expect("edge should be valid"),
            OutcomeGroupLineageEdge::new(
                second.id,
                first.id,
                OutcomeGroupAdjustment::QuestionChange,
                1,
            )
            .expect("edge should be valid"),
        ];

        assert_eq!(
            validate_outcome_group_lineage(&edges),
            Err(ModelError::OutcomeGroupLineageInvalid {
                field: "lineage cycle"
            })
        );
    }

    #[test]
    fn risk_pool_should_sort_and_validate_members() {
        let first = binary_group("Will it rain?");
        let second = binary_group("Will it snow?");
        let mut expected = vec![first.id, second.id];
        expected.sort();
        let pool = OutcomeRiskPool::new(
            usdc(),
            vec![second.id, first.id],
            vec![conversion(&first, 1, &second, 0)],
        )
        .expect("pool should be valid");

        assert_eq!(pool.groups, expected);
        assert!(pool.contains(&first.id));
    }

    #[test]
    fn risk_pool_should_reject_a_conversion_outside_its_members() {
        let first = binary_group("Will it rain?");
        let second = binary_group("Will it snow?");
        let third = binary_group("Will it hail?");

        assert!(matches!(
            OutcomeRiskPool::new(
                usdc(),
                vec![first.id, second.id],
                vec![conversion(&third, 0, &second, 0)],
            ),
            Err(ModelError::OutcomeConversionUnresolved { .. })
        ));
    }

    #[test]
    fn risk_pool_should_reject_a_conversion_cycle() {
        let first = binary_group("Will it rain?");
        let second = binary_group("Will it snow?");

        assert_eq!(
            OutcomeRiskPool::new(
                usdc(),
                vec![first.id, second.id],
                vec![
                    conversion(&first, 0, &second, 0),
                    conversion(&second, 0, &first, 0),
                ],
            ),
            Err(ModelError::OutcomeConversionCycle)
        );
    }

    #[test]
    fn risk_pool_lineage_should_reject_a_duplicate_successor() {
        let first = binary_group("Will it rain?");
        let second = binary_group("Will it snow?");
        let base = OutcomeRiskPoolId::from_identity_value(&format!("v1/{}", "1".repeat(64)))
            .expect("base identity");
        let successor = OutcomeRiskPoolId::from_identity_value(&format!("v1/{}", "2".repeat(64)))
            .expect("successor identity");
        let edges = vec![
            OutcomeRiskPoolLineageEdge::new(
                base,
                successor,
                OutcomeRiskPoolAdjustment::GroupMembershipChange,
                0,
            )
            .expect("edge should be valid"),
            OutcomeRiskPoolLineageEdge::new(
                OutcomeRiskPoolId::from_identity_value(&format!("v1/{}", "3".repeat(64)))
                    .expect("predecessor identity"),
                successor,
                OutcomeRiskPoolAdjustment::GroupMembershipChange,
                1,
            )
            .expect("edge should be valid"),
        ];

        assert_eq!(
            super::validate_outcome_risk_pool_lineage(&edges),
            Err(ModelError::OutcomeRiskPoolLineageInvalid {
                field: "duplicate lineage successor"
            })
        );
        assert_eq!(edges[0].predecessor(), base.as_str());
        assert_ne!(first.id, second.id);
    }

    #[test]
    fn binary_settlement_rules_should_accept_only_zero_or_one() {
        let group = binary_group("Will it rain?");
        let rule = SettlementFractionRule::Binary;
        let half = Fraction::new("0.5".parse::<Ratio>().expect("ratio should parse"))
            .expect("half is a fraction");

        assert!(rule.validate_fraction(&group.id, Fraction::ZERO).is_ok());
        assert!(rule.validate_fraction(&group.id, Fraction::ONE).is_ok());
        assert!(matches!(
            rule.validate_fraction(&group.id, half),
            Err(ModelError::OutcomeSettlementFractionInvalid { .. })
        ));
        assert!(
            SettlementFractionRule::AnyFraction
                .validate_fraction(&group.id, half)
                .is_ok()
        );
    }

    #[test]
    fn fractional_group_should_require_a_leg_for_every_tradable_side() {
        let group = exclusive_group_with(
            "Who wins?",
            &["a", "b", "c"],
            SelectionCardinality::AtMostOne,
        );
        let fractions = vec![Fraction::ZERO, Fraction::ZERO, Fraction::ZERO];
        let complete = settlement(&group, &fractions, 1_950);

        assert_eq!(complete.legs.len(), group.tradable_legs().len());
        assert!(
            complete.validate_against(&group).is_ok(),
            "a group whose every side settles at zero is still fully covered"
        );

        let missing = OutcomeSettlement {
            group: group.id,
            legs: complete.legs[1..].to_vec(),
            settled_at: 1_950,
            source: None,
        };
        assert!(matches!(
            missing.validate_against(&group),
            Err(ModelError::OutcomeSettlementIncomplete { .. })
        ));
    }

    #[test]
    fn a_group_should_belong_to_at_most_one_risk_pool() {
        let first = binary_group("Will it rain?");
        let second = binary_group("Will it snow?");
        let third = binary_group("Will it hail?");
        let left = OutcomeRiskPool::new(
            usdc(),
            vec![first.id, second.id],
            vec![conversion(&first, 0, &second, 0)],
        )
        .expect("pool should be valid");
        let right = OutcomeRiskPool::new(
            usdc(),
            vec![second.id, third.id],
            vec![conversion(&second, 0, &third, 0)],
        )
        .expect("pool should be valid");

        assert_eq!(
            validate_risk_pool_membership(std::slice::from_ref(&left)),
            Ok(())
        );
        assert!(matches!(
            validate_risk_pool_membership(&[left, right]),
            Err(ModelError::OutcomeRiskPoolIncomplete { .. })
        ));
    }

    #[test]
    fn a_group_successor_should_require_a_risk_pool_successor() {
        let first = binary_group("Will it rain?");
        let second = binary_group("Will it snow?");
        let successor = binary_group("Will it rain tomorrow?");
        let pool = OutcomeRiskPool::new(
            usdc(),
            vec![first.id, second.id],
            vec![conversion(&first, 0, &second, 0)],
        )
        .expect("pool should be valid");
        let group_edges = vec![
            OutcomeGroupLineageEdge::new(
                first.id,
                successor.id,
                OutcomeGroupAdjustment::QuestionChange,
                10,
            )
            .expect("edge should be valid"),
        ];

        assert!(matches!(
            validate_pool_successors(&group_edges, &[], std::slice::from_ref(&pool)),
            Err(ModelError::OutcomeRiskPoolLineageInvalid { .. })
        ));

        let successor_pool = OutcomeRiskPool::new(
            usdc(),
            vec![successor.id, second.id],
            vec![conversion(&successor, 0, &second, 0)],
        )
        .expect("successor pool should be valid");

        let pool_edges = vec![
            OutcomeRiskPoolLineageEdge::new(
                pool.id,
                successor_pool.id,
                OutcomeRiskPoolAdjustment::GroupMembershipChange,
                20,
            )
            .expect("edge should be valid"),
        ];
        assert!(
            validate_pool_successors(&group_edges, &pool_edges, std::slice::from_ref(&pool))
                .is_ok()
        );
    }

    #[test]
    fn risk_pool_topology_should_reject_a_cycle_inside_its_members() {
        let first = binary_group("Will it rain?");
        let second = binary_group("Will it snow?");

        assert!(matches!(
            OutcomeRiskPool::new(
                usdc(),
                vec![first.id, second.id],
                vec![
                    conversion(&first, 0, &second, 0),
                    conversion(&second, 1, &first, 1),
                ],
            ),
            Err(ModelError::OutcomeConversionCycle)
        ));
    }

    #[test]
    fn settlement_should_cover_every_tradable_side() {
        let group = binary_group("Will it rain?");
        let complete = settlement(&group, &[Fraction::ONE, Fraction::ZERO], 1_950);
        let incomplete = OutcomeSettlement {
            group: group.id,
            legs: vec![complete.legs[0].clone()],
            settled_at: 1_950,
            source: None,
        };

        assert!(complete.validate_against(&group).is_ok());
        assert!(matches!(
            incomplete.validate_against(&group),
            Err(ModelError::OutcomeSettlementIncomplete { .. })
        ));
    }

    #[test]
    fn settlement_should_reject_non_complementary_binary_fractions() {
        let group = binary_group("Will it rain?");

        assert!(matches!(
            settlement(&group, &[Fraction::ONE, Fraction::ONE], 1_950).validate_against(&group),
            Err(ModelError::OutcomeSettlementFractionInvalid { .. })
        ));
        assert!(
            settlement(&group, &[Fraction::ONE, Fraction::ZERO], 1_950)
                .validate_against(&group)
                .is_ok()
        );
    }

    #[test]
    fn mutually_exclusive_settlement_should_select_exactly_one_member() {
        let group = exclusive_group("Who wins?", &["a", "b", "c"]);
        let fractions: Vec<(smol_str::SmolStr, Fraction)> = group
            .members
            .iter()
            .map(|member| (member.label.clone(), Fraction::ZERO))
            .collect();
        let with_winner = |winner: Option<usize>| {
            let mut fractions = fractions.clone();
            if let Some(winner) = winner {
                fractions[winner].1 = Fraction::ONE;
            }
            fractions
        };
        let select = |fractions: &[(smol_str::SmolStr, Fraction)]| OutcomeSettlement {
            group: group.id,
            legs: group
                .members
                .iter()
                .map(|member| {
                    let fraction = fractions
                        .iter()
                        .find(|(label, _)| label == &member.label)
                        .map(|(_, fraction)| *fraction)
                        .unwrap_or(Fraction::ZERO);
                    OutcomeSettlementLeg {
                        member: member.id,
                        side: member.sides[0],
                        settle_fraction: fraction,
                    }
                })
                .collect(),
            settled_at: 1_950,
            source: None,
        };
        let none = select(&with_winner(None));
        let one = select(&with_winner(Some(1)));
        let mut two_fractions = with_winner(Some(1));
        two_fractions[2].1 = Fraction::ONE;
        let two = select(&two_fractions);

        assert!(matches!(
            none.validate_against(&group),
            Err(ModelError::OutcomeSettlementFractionInvalid { .. })
        ));
        assert!(matches!(
            two.validate_against(&group),
            Err(ModelError::OutcomeSettlementFractionInvalid { .. })
        ));
        assert!(one.validate_against(&group).is_ok());
    }

    #[test]
    fn expiry_should_bound_the_resolution_rule() {
        let rule = ResolutionRule::FixedAt { at: 2_100 };

        assert_eq!(
            rule.validate(&expiry()),
            Err(ModelError::InvalidTimeOrder {
                field: "resolution.at",
                limit: "expiration_at"
            })
        );
    }

    #[test]
    fn event_identity_should_not_drift_with_membership() {
        let first = binary_group("Will it rain?");
        let second = binary_group("Will it snow?");
        let event = OutcomeEvent::new(
            ResolutionSourceId::new("polymarket").expect("source should be valid"),
            IdentifierValue::new("event-1").expect("event should be valid"),
            vec![first.id],
            Vec::new(),
        )
        .expect("event should be valid");
        let refreshed = OutcomeEvent::new(
            ResolutionSourceId::new("polymarket").expect("source should be valid"),
            IdentifierValue::new("event-1").expect("event should be valid"),
            vec![second.id, first.id],
            Vec::new(),
        )
        .expect("event should be valid");
        let duplicated = OutcomeEvent::new(
            ResolutionSourceId::new("polymarket").expect("source should be valid"),
            IdentifierValue::new("event-1").expect("event should be valid"),
            vec![first.id, first.id],
            Vec::new(),
        );

        assert_eq!(event.id, refreshed.id);
        assert_eq!(
            duplicated,
            Err(ModelError::UnsortedCollection {
                field: "outcome_event.groups"
            })
        );
        let mut expected = vec![first.id, second.id];
        expected.sort();
        assert_eq!(refreshed.groups, expected);
    }

    #[test]
    fn risk_pool_terms_should_change_the_identity() {
        let first = binary_group("Will it rain?");
        let second = binary_group("Will it snow?");
        let third = binary_group("Will it hail?");
        let two_groups = OutcomeRiskPool::new(usdc(), vec![first.id, second.id], Vec::new())
            .expect("pool should be valid");
        let three_groups =
            OutcomeRiskPool::new(usdc(), vec![first.id, second.id, third.id], Vec::new())
                .expect("pool should be valid");

        assert_ne!(two_groups.id, three_groups.id);
    }

    #[test]
    fn leg_label_should_render_all_three_identities() {
        let group = binary_group("Will it rain?");
        let leg = OutcomeLegRef {
            group: group.id,
            member: group.members[0].id,
            side: group.members[0].sides[0],
        };
        let rendered = super::leg_label(&leg);

        assert_eq!(
            rendered.as_str(),
            format!("{}/{}/{}", leg.group, leg.member, leg.side)
        );
        assert!(rendered.contains(group.id.as_str().as_str()));
        assert!(rendered.contains(group.members[0].id.as_str().as_str()));
        assert!(rendered.contains(group.members[0].sides[0].as_str().as_str()));
    }
}
