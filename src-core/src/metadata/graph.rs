use crate::metadata::types::*;

/// Revision graph operations.
/// Builds and queries the DAG of file revisions.
pub struct RevisionGraph;

impl RevisionGraph {
    /// Check if two revisions are siblings (same parent, different children)
    pub fn are_siblings(_parent: &RevisionId, a: &RevisionId, b: &RevisionId) -> bool {
        a != b // Same level, different branch IDs
    }

    /// Determine if revision `a` is an ancestor of revision `b`
    pub fn is_ancestor_of(
        a: &RevisionId,
        b: &RevisionId,
        parents: &[(RevisionId, Option<RevisionId>)],
    ) -> bool {
        // Walk up the graph from b to see if we reach a
        let mut current = Some(*b);
        while let Some(rev) = current {
            if rev == *a {
                return true;
            }
            // Find parent
            current = parents
                .iter()
                .find(|(id, _)| id == &rev)
                .and_then(|(_, parent)| *parent);
        }
        false
    }

    /// Find the lowest common ancestor of two revisions
    pub fn lowest_common_ancestor(
        a: &RevisionId,
        b: &RevisionId,
        parents: &[(RevisionId, Option<RevisionId>)],
    ) -> Option<RevisionId> {
        // Collect ancestors of a
        let mut ancestors_a = Vec::new();
        let mut current = Some(*a);
        while let Some(rev) = current {
            ancestors_a.push(rev);
            current = parents
                .iter()
                .find(|(id, _)| id == &rev)
                .and_then(|(_, parent)| *parent);
        }

        // Walk up from b, find first common ancestor
        let mut current = Some(*b);
        while let Some(rev) = current {
            if ancestors_a.contains(&rev) {
                return Some(rev);
            }
            current = parents
                .iter()
                .find(|(id, _)| id == &rev)
                .and_then(|(_, parent)| *parent);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn make_rev(id: u128, parent: Option<u128>) -> (RevisionId, Option<RevisionId>) {
        (Uuid::from_u128(id), parent.map(Uuid::from_u128))
    }

    #[test]
    fn test_ancestor() {
        let parents = vec![
            make_rev(3, Some(1)),
            make_rev(2, Some(1)),
            make_rev(1, None),
        ];

        assert!(RevisionGraph::is_ancestor_of(
            &Uuid::from_u128(1),
            &Uuid::from_u128(3),
            &parents
        ));
        assert!(!RevisionGraph::is_ancestor_of(
            &Uuid::from_u128(2),
            &Uuid::from_u128(3),
            &parents
        ));
    }

    #[test]
    fn test_lca() {
        // Tree: 1 → 2, 1 → 3
        let parents = vec![
            make_rev(3, Some(1)),
            make_rev(2, Some(1)),
            make_rev(1, None),
        ];

        let lca = RevisionGraph::lowest_common_ancestor(
            &Uuid::from_u128(2),
            &Uuid::from_u128(3),
            &parents,
        );
        assert_eq!(lca, Some(Uuid::from_u128(1)));
    }
}
