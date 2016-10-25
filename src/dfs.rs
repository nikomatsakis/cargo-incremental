use git2::{Commit, Oid};
use std::collections::HashSet;
use std::hash::Hash;
use std::io::prelude::*;

use super::util::short_id;

/// Given a start and end point, returns a linear series of commits to traverse.
/// The correct ordering here is not always clear; we adopt reverse-post-order,
/// which yields "reasonable" results.
///
/// Example:
///
///    A
///    |\
///    B C
///    |/
///    D
///
/// Here you have two branches (B and C) that were active in parallel from a common
/// starting point D. RPO will yield D, B, C, A (or D, C, B, A) which seems ok.
///
/// Some complications:
///
/// - The `start` point may not be the common ancestor we need. e.g.,
///   in the above graph, what should we do if the start point is B
///   and end point is A? What we do is to yield B, C, A. We do this
///   by excluding all nodes that are reachable from the start
///   point. The reason for this is that if you test
///   `master~3..master` and then `master~10..master~3` you will
///   basically test all commits which landed into master at various
///   points.  If we omitted things that could not reach `start`
///   (e.g., walking only B, A in in our example) then we might just
///   miss commit C altogether.
pub fn find_path<NODE: DfsNode>(start: Option<NODE>,
                                end: NODE)
                                -> Vec<NODE> {
    debug!("find_path(start={}, end={})",
        start.as_ref().map(DfsNode::human_readable_id).unwrap_or("None".to_string()),
        end.human_readable_id());

    let start_id = start.as_ref().map(|c| c.id());

    // Collect all nodes reachable from the start.
    let mut reachable_from_start = start.map(|c| walk(c, |_| true, |_| ()))
        .unwrap_or(HashSet::new());

    if let Some(start_id) = start_id {
        reachable_from_start.remove(&start_id);
    }

    // Walk backwards from end; stop when we reach any thing reachable
    // from start (except for start itself, walk that). Accumulate
    // completed notes into `commits`.
    let mut commits = vec![];
    walk(end,
         |c| !reachable_from_start.contains(&c.id()),
         |c| commits.push(c));

    commits
}

fn walk<NODE, PRE, POST>(
        start: NODE,
        mut check: PRE,
        mut complete: POST) -> HashSet<NODE::Id>
    where NODE: DfsNode,
          PRE: FnMut(&NODE) -> bool,
          POST: FnMut(NODE)
{
    let mut visited = HashSet::new();
    let mut stack = vec![DfsFrame::new(start)];
    while let Some(mut frame) = stack.pop() {
        let next_parent = frame.next_parent;
        if next_parent == frame.num_parents {
            complete(frame.node);
        } else {
            let node = frame.node.parent(next_parent);
            frame.next_parent += 1;
            stack.push(frame);
            if visited.insert(node.id()) {
                if check(&node) {
                    stack.push(DfsFrame::new(node));
                }
            }
        }
    }
    visited
}

struct DfsFrame<NODE: DfsNode> {
    node: NODE,
    next_parent: usize,
    num_parents: usize,
}

impl<NODE: DfsNode> DfsFrame<NODE> {
    fn new(node: NODE) -> Self {
        let num_parents = node.num_parents();
        DfsFrame {
            node: node,
            next_parent: 0,
            num_parents: num_parents,
        }
    }
}

pub trait DfsNode
{
    type Id: Eq + Hash;

    fn id(&self) -> Self::Id;
    fn human_readable_id(&self) -> String;
    fn parent(&self, index: usize) -> Self;
    fn num_parents(&self) -> usize;
}

impl<'repo> DfsNode for Commit<'repo> {
    type Id = Oid;

    fn id(&self) -> Oid {
        self.id()
    }

    fn human_readable_id(&self) -> String {
        short_id(self)
    }

    fn parent(&self, index: usize) -> Commit<'repo> {
        match self.parent(index) {
            Ok(p) => p,
            Err(err) => {
                error!("unable to load parent {} of commit {}: {}",
                       index,
                       short_id(self),
                       err)
            }
        }
    }

    fn num_parents(&self) -> usize {
        self.parents().len()
    }
}

#[cfg(test)]
mod test {
    use std::fmt;
    use super::{DfsNode, find_path};

    #[derive(Eq, PartialEq)]
    struct TestNode<'a> {
        id: char,
        parents: Vec<&'a TestNode<'a>>
    }

    impl<'a> fmt::Debug for TestNode<'a> {
        fn fmt(&self, formatter: &mut fmt::Formatter) -> Result<(), fmt::Error> {
            write!(formatter, "{}", self.id)
        }
    }

    impl<'a> TestNode<'a> {
        fn new(id: char, parents: &[&'a TestNode<'a>]) -> TestNode<'a> {
            TestNode {
                id: id,
                parents: parents.to_vec(),
            }
        }
    }

    impl<'a> DfsNode for &'a TestNode<'a> {
        type Id = char;

        fn id(&self) -> Self::Id {
            self.id
        }
        fn human_readable_id(&self) -> String {
            format!("{}", self.id)
        }

        fn parent(&self, index: usize) -> Self {
            self.parents[index]
        }
        fn num_parents(&self) -> usize {
            self.parents.len()
        }
    }

    #[test]
    fn test() {
        //
        //    a
        //    |
        //    b
        //   / \
        //  c   d
        //   \ /
        //    e
        //    |
        //    f
        //    |
        //    g
        //
        // parent relationship goes from top to bottom (e.g. B is parent of A)

        let g = TestNode::new('g', &[]);
        let f = TestNode::new('f', &[&g]);
        let e = TestNode::new('e', &[&f]);
        let d = TestNode::new('d', &[&e]);
        let c = TestNode::new('c', &[&e]);
        let b = TestNode::new('b', &[&c, &d]);
        let a = TestNode::new('a', &[&b]);

        assert_eq!(find_path(Some(&b), &a), vec![&b, &a]);
        assert_eq!(find_path(Some(&e), &a), vec![&e, &c, &d, &b, &a]);
        assert_eq!(find_path(Some(&g), &f), vec![&g, &f]);
        assert_eq!(find_path(None, &f), vec![&g, &f]);
        assert_eq!(find_path(Some(&d), &b), vec![&c, &d, &b]);
    }
}
