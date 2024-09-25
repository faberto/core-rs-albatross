use std::sync::Arc;

use parking_lot::RwLock;

use crate::{
    contribution::AggregatableContribution,
    evaluator::VerificationError::{
        InvalidContributors, InvalidFullAggregate, InvalidIndividualContribution, InvalidLevel,
        InvalidOrigin,
    },
    identity::{Identity, IdentityRegistry, WeightRegistry},
    partitioner::Partitioner,
    protocol::Protocol,
    store::ContributionStore,
    update::LevelUpdate,
    Identifier,
};

/// Trait for scoring or evaluating a contribution or signature.
pub trait Evaluator<TId, TProtocol>
where
    TId: Identifier,
    TProtocol: Protocol<TId>,
    Self: Send + Sync,
{
    /// Takes an unverified contribution and scores it in terms of usefulness with
    ///
    /// `0` being not useful at all, can be discarded.
    /// `>0` being more useful the bigger the number.
    fn evaluate(&self, signature: &TProtocol::Contribution, level: usize, id: TId) -> usize;

    /// Returns whether a level contains a specific peer ID.
    fn verify(&self, msg: &LevelUpdate<TProtocol::Contribution>) -> Result<(), VerificationError>;
}

/// A signature counts as it was signed N times, where N is the signers weight
#[derive(Debug)]
pub struct WeightedVote<TId, TProtocol>
where
    TId: Identifier,
    TProtocol: Protocol<TId>,
{
    /// The contribution store.
    store: Arc<RwLock<TProtocol::Store>>,

    /// Registry that maps the signers to the weight they have in a signature.
    pub weights: Arc<TProtocol::Registry>,

    /// Partitioner that registers the handel levels and its IDs.
    partitioner: Arc<TProtocol::Partitioner>,
}

impl<TId, TProtocol> WeightedVote<TId, TProtocol>
where
    TId: Identifier,
    TProtocol: Protocol<TId>,
{
    /// If a contribution completes a level this is the base score
    const COMPLETES_LEVEL_BASE_SCORE: usize = 1_000_000;

    /// For contribution which complete a level this is a penalty multiplied with the level, resulting
    /// in higher levels having lower scores.
    const COMPLETES_LEVEL_LEVEL_PENALTY: usize = 10;

    /// If a contribution improves the best score on its level this is the base score
    const IMPROVEMENT_BASE_SCORE: usize = 100_000;

    /// For a contribution which improves the best score this is the penalty per level, resulting
    /// in higher levels having a lower score.
    const IMPROVEMENT_LEVEL_PENALTY: usize = 100;

    /// For a contribution which improves the best score this is a bonus added to th score per signature added.
    const IMPROVEMENT_ADDED_SIG_BONUS: usize = 10;

    pub fn new(
        store: Arc<RwLock<TProtocol::Store>>,
        weights: Arc<TProtocol::Registry>,
        partitioner: Arc<TProtocol::Partitioner>,
    ) -> Self {
        Self {
            store,
            weights,
            partitioner,
        }
    }
}

#[derive(Debug)]
pub enum VerificationError {
    InvalidLevel {
        level: usize,
        num_levels: usize,
    },
    InvalidFullAggregate {
        weight: usize,
        expected_weight: usize,
    },
    InvalidOrigin {
        origin: usize,
        allowed_contributors: Identity,
    },
    InvalidIndividualContribution {
        num_contributors: usize,
        contains_origin: bool,
    },
    InvalidContributors {
        contributors: Identity,
        allowed_contributors: Identity,
    },
}

impl<TId, TProtocol> Evaluator<TId, TProtocol> for WeightedVote<TId, TProtocol>
where
    TId: Identifier,
    TProtocol: Protocol<TId>,
{
    /// Takes an unverified contribution and scores it in terms of usefulness with
    ///
    /// `0` being not useful at all, can be discarded.
    /// `>0` being more useful the bigger the number.
    fn evaluate(&self, contribution: &TProtocol::Contribution, level: usize, id: TId) -> usize {
        // Special case for final aggregations, full contribution is already checked.
        if level == self.partitioner.levels() {
            return usize::MAX;
        }

        let store = self.store.read();

        // Calculate the identity represented in the contribution.
        let identity = self.weights.signers_identity(&contribution.contributors());

        // Empty or faulty signatures get a score of 0
        if identity.is_empty() {
            return 0;
        }

        // For contributions with a single signer, check if it is already known.
        if identity.len() == 1 && store.individual_signature(level, &identity).is_some() {
            // If we already know it for this level, score it as 0
            return 0;
        }

        // Number of identities at `level`, sort of maximum receivable individual contributions
        let level_identity_count = self.partitioner.level_size(level);

        // The current best contribution stored for `level`.
        let best_contribution = store.best(level);

        if let Some(best_contribution) = best_contribution {
            let best_contributors = self
                .weights
                .signers_identity(&best_contribution.contributors());

            // Check if the best signature for that level is already complete
            if level_identity_count == best_contributors.len() {
                return 0;
            }

            // Check if the best signature is strictly better than the new one
            if best_contributors.is_superset_of(&identity) {
                return 0;
            }
        }

        // Compute bitset of signers combined with all (verified) individual signatures that we have.
        // Allow intersection here as all individual signatures are stored individually.
        let mut with_individuals = identity.clone();
        with_individuals.combine(store.individual_verified(level), true);

        // ---------------------------------------------

        let (new_total, added_sigs, combined_sigs) = if let Some(best_signature) = best_contribution
        {
            let best_contributors = self
                .weights
                .signers_identity(&best_signature.contributors());

            if identity.intersection_size(&best_contributors) > 0 {
                // The contribution we got cannot be merged into the best one we already have, as they overlap.
                // Best thing we can do is merge individual contributions into it.
                let new_total = with_individuals.len();
                (
                    // The new contribution combined with all already verified individuals not yet present in the new one.
                    new_total,
                    (new_total as isize) - best_contributors.len() as isize,
                    new_total - identity.len(),
                )
            } else {
                // The signatures can be combined, so the resulting signature will have the signers of both,
                // as well as individual signatures.

                let mut final_sig = with_individuals.clone();
                // Intersections must be allowed as individuals are already present on the left side, and potentially
                // part of the right side.
                final_sig.combine(&best_contributors, true);

                // Needed to find out how many individuals are present in final_sig
                let mut without_individuals = best_contributors.clone();
                without_individuals.combine(&identity, false);

                let new_total = final_sig.len();
                let combined_sigs = (final_sig ^ without_individuals).len();
                (
                    new_total,
                    new_total as isize - best_contributors.len() as isize,
                    combined_sigs,
                )
            }
        } else {
            // Currently there is no best signature for this level. The new signature will become the best.
            // Best is the new signature with the individual signatures. However, if there are individual
            // signatures for this level there should also be a best signature.
            if with_individuals.len() != identity.len() {
                log::warn!(
                    %id,
                    ?level,
                    ?identity,
                    individuals = ?store.individual_verified(level),
                    "No best contribution found, even though there are individuals",
                );
            }
            let new_total = with_individuals.len();

            (
                new_total,                  // this should be identical to identity.len()
                new_total as isize,         // This will be a positive number
                new_total - identity.len(), // This should be 0
            )
        };

        // Compute score
        if added_sigs <= 0 {
            // return `signature_weight` for an individual signature, otherwise 0 as the signature is useless
            if identity.len() == 1 {
                return self.weights.signature_weight(contribution).unwrap_or(0);
            }
            return 0;
        }

        if new_total == level_identity_count {
            // The signature will complete the level it is on.
            // These signatures are the most valuable, with early levels being more valuable than later ones.
            // The less signatures are added by combining with individual ones, the better.
            return Self::COMPLETES_LEVEL_BASE_SCORE
                - level * Self::COMPLETES_LEVEL_LEVEL_PENALTY
                - combined_sigs;
        }

        // The signature makes the best signature better, but does not complete a level.
        // Make it so it will be better than in individual but worse than those which complete a level.
        // Favor earlier levels over later levels.
        // Favor those which add more signatures but out of them favor those with less individual merges.
        Self::IMPROVEMENT_BASE_SCORE - level * Self::IMPROVEMENT_LEVEL_PENALTY
            + added_sigs as usize * Self::IMPROVEMENT_ADDED_SIG_BONUS
            - combined_sigs
    }

    fn verify(&self, msg: &LevelUpdate<TProtocol::Contribution>) -> Result<(), VerificationError> {
        // Check that the level is within bounds.
        let level = msg.level as usize;
        let num_levels = self.partitioner.levels();
        if level > num_levels || level < 1 {
            return Err(InvalidLevel { level, num_levels });
        }

        // Special case for full aggregations, which are sent at level `num_levels`.
        // They are only valid if they contain all signers.
        let contributors = self.weights.signers_identity(&msg.aggregate.contributors());
        if level == num_levels {
            let weight = contributors.len();
            let expected_weight = self.partitioner.size();
            if weight != expected_weight {
                return Err(InvalidFullAggregate {
                    weight,
                    expected_weight,
                });
            }

            return Ok(());
        }

        // Get the valid contributors for this level.
        // We have validated the level, so we can expect a range here.
        let allowed_contributors = self
            .partitioner
            .identities_on(level)
            .expect("Identities should exist");

        // Check that the message origin is a valid contributor.
        let origin = msg.origin as usize;
        if !allowed_contributors.contains(origin) {
            return Err(InvalidOrigin {
                origin,
                allowed_contributors,
            });
        }

        // Check that the signer of the individual contribution corresponds to the message origin.
        if let Some(individual) = &msg.individual {
            let individual_contributors = self.weights.signers_identity(&individual.contributors());
            let num_contributors = individual_contributors.len();
            let contains_origin = individual_contributors.contains(origin);
            if num_contributors != 1 || !contains_origin {
                return Err(InvalidIndividualContribution {
                    num_contributors,
                    contains_origin,
                });
            }
        }

        // Check that all contributors to the aggregate contribution are allowed on this level.
        if !allowed_contributors.is_superset_of(&contributors) {
            return Err(InvalidContributors {
                contributors,
                allowed_contributors,
            });
        }

        Ok(())
    }
}
