use git2::{Commit, Oid};
use std::collections::HashSet;
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
pub fn find_path<'obj, 'repo>(start: Option<Commit<'repo>>,
                              end: Commit<'repo>)
                              -> Vec<Commit<'repo>> {
    debug!("find_path(start={}, end={})",
        start.as_ref().map(short_id).unwrap_or("None".to_string()),
        short_id(&end));

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

    // `commits` is now post-order; reverse it, and return.
    commits.reverse();

    commits
}

fn walk<'repo, PRE, POST>(start: Commit<'repo>, mut check: PRE, mut complete: POST) -> HashSet<Oid>
    where PRE: FnMut(&Commit<'repo>) -> bool,
          POST: FnMut(Commit<'repo>)
{
    let mut visited = HashSet::new();
    let mut stack = vec![DfsFrame::new(start)];
    while let Some(mut frame) = stack.pop() {
        let next_parent = frame.next_parent;
        if next_parent == frame.num_parents {
            complete(frame.commit);
        } else {
            let commit = match frame.commit.parent(next_parent) {
                Ok(p) => p,
                Err(err) => {
                    error!("unable to load parent {} of commit {}: {}",
                           next_parent,
                           short_id(&frame.commit),
                           err)
                }
            };
            frame.next_parent += 1;
            stack.push(frame);
            if visited.insert(commit.id()) {
                if check(&commit) {
                    stack.push(DfsFrame::new(commit));
                }
            }
        }
    }
    visited
}

struct DfsFrame<'repo> {
    commit: Commit<'repo>,
    next_parent: usize,
    num_parents: usize,
}

impl<'repo> DfsFrame<'repo> {
    fn new(commit: Commit<'repo>) -> Self {
        let num_parents = commit.parents().len();
        DfsFrame {
            commit: commit,
            next_parent: 0,
            num_parents: num_parents,
        }
    }
}
