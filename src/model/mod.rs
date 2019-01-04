mod cluster;
pub mod eval;
pub mod liblinear;
pub mod train;

use crate::{DenseVec, DenseVecView, Index, IndexValueVec, SparseMat};
use hashbrown::HashMap;
use itertools::Itertools;
use log::info;
use serde::{Deserialize, Serialize};
use std::io;
use std::mem::swap;

/// Model training hyper-parameters.
pub type TrainHyperParam = train::HyperParam;

/// A Parabel model, which contains a forest of trees.
#[derive(Debug, Serialize, Deserialize)]
pub struct Model {
    trees: Vec<Tree>,
    n_features: usize,
    hyper_parm: TrainHyperParam,
}

impl Model {
    /// Returns a ranked list of predictions for the given input example.
    ///
    /// # Arguments
    ///
    /// * `feature_vec` - An input vector for prediction, assumed to be ordered by indices and have
    /// no duplicate or out-of-range indices
    /// * `beam_size` - Beam size for beam search.
    pub fn predict(&self, feature_vec: &[(Index, f32)], beam_size: usize) -> IndexValueVec {
        let feature_vec = self.prepare_dense_feature_vec(feature_vec);
        let mut label_to_total_score = HashMap::<Index, f32>::new();
        let tree_predictions: Vec<_> = self
            .trees
            .iter()
            .map(|tree| {
                tree.predict(
                    feature_vec.view(),
                    beam_size,
                    self.hyper_parm.linear.loss_type,
                )
            })
            .collect();
        for label_score_pairs in tree_predictions {
            for (label, score) in label_score_pairs {
                let total_score = label_to_total_score.entry(label).or_insert(0.);
                *total_score += score;
            }
        }

        let mut label_score_pairs = label_to_total_score
            .iter()
            .map(|(&label, &total_score)| (label, total_score / self.trees.len() as f32))
            .collect_vec();
        label_score_pairs
            .sort_unstable_by(|(_, score1), (_, score2)| score2.partial_cmp(score1).unwrap());
        label_score_pairs
    }

    /// Normalize and densify the sparse feature vector to make prediction more efficient.
    fn prepare_dense_feature_vec(&self, sparse_vec: &[(Index, f32)]) -> DenseVec {
        let mut dense_vec = DenseVec::zeros(self.n_features + 1);
        let norm = sparse_vec
            .iter()
            .map(|(_, v)| v.powi(2))
            .sum::<f32>()
            .sqrt();
        for &(index, value) in sparse_vec {
            dense_vec[index as usize] = value / norm; // l2-normalized
        }
        dense_vec[self.n_features] = 1.; // bias
        dense_vec
    }

    /// Serialize model.
    pub fn save<W: io::Write>(&self, writer: W) -> io::Result<()> {
        info!("Saving model...");
        let start_t = time::precise_time_s();

        bincode::serialize_into(writer, self)
            .or_else(|e| Err(io::Error::new(io::ErrorKind::Other, e)))?;

        info!(
            "Model saved; it took {:.2}s",
            time::precise_time_s() - start_t
        );
        Ok(())
    }

    /// Deserialize model.
    pub fn load<R: io::Read>(reader: R) -> io::Result<Self> {
        info!("Loading model...");
        let start_t = time::precise_time_s();

        let model: Self = bincode::deserialize_from(reader)
            .or_else(|e| Err(io::Error::new(io::ErrorKind::Other, e)))?;
        info!(
            "Model loaded; it took {:.2}s",
            time::precise_time_s() - start_t
        );
        Ok(model)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Tree {
    root: TreeNode,
}

#[derive(Debug, Serialize, Deserialize)]
enum TreeNode {
    BranchNode {
        weight_matrix: SparseMat,
        children: Vec<TreeNode>,
    },
    LeafNode {
        weight_matrix: SparseMat,
        labels: Vec<Index>,
    },
}

impl TreeNode {
    fn is_leaf(&self) -> bool {
        if let TreeNode::LeafNode { .. } = self {
            true
        } else {
            false
        }
    }
}

impl Tree {
    fn predict(
        &self,
        feature_vec: DenseVecView,
        beam_size: usize,
        liblinear_loss_type: liblinear::LossType,
    ) -> IndexValueVec {
        assert!(beam_size > 0);
        let mut curr_level = Vec::<(&TreeNode, f32)>::with_capacity(beam_size * 2);
        let mut next_level = Vec::<(&TreeNode, f32)>::with_capacity(beam_size * 2);

        curr_level.push((&self.root, 0.));
        loop {
            assert!(!curr_level.is_empty());

            if curr_level.len() > beam_size {
                curr_level.sort_unstable_by(|(_, score1), (_, score2)| {
                    score2.partial_cmp(score1).unwrap()
                });
                curr_level.truncate(beam_size);
            }

            // Iterate until we reach the leaves
            if curr_level.first().unwrap().0.is_leaf() {
                break;
            }

            next_level.clear();
            for &(node, node_score) in &curr_level {
                match node {
                    TreeNode::BranchNode {
                        weight_matrix,
                        children,
                    } => {
                        let mut child_scores = liblinear::predict_with_classifier_group(
                            feature_vec,
                            weight_matrix.view(),
                            liblinear_loss_type,
                        );
                        assert_eq!(child_scores.len(), children.len());
                        for child_score in &mut child_scores {
                            *child_score += node_score;
                        }

                        next_level.extend(children.iter().zip_eq(child_scores.into_iter()));
                    }
                    _ => unreachable!("The tree is not a complete binary tree."),
                }
            }

            swap(&mut curr_level, &mut next_level);
        }

        curr_level
            .iter()
            .flat_map(|&(leaf, leaf_score)| match leaf {
                TreeNode::LeafNode {
                    weight_matrix,
                    labels,
                } => {
                    let mut label_scores = liblinear::predict_with_classifier_group(
                        feature_vec.view(),
                        weight_matrix.view(),
                        liblinear_loss_type,
                    );
                    for label_score in &mut label_scores {
                        *label_score = (*label_score + leaf_score).exp();
                    }
                    labels
                        .iter()
                        .cloned()
                        .zip_eq(label_scores.into_iter())
                        .collect_vec()
                }
                _ => unreachable!("The tree is not a complete binary tree."),
            })
            .collect_vec()
    }
}
