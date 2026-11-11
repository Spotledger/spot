//! `seed::documents`: creates sample-app documents through a node's own API, for later tasks
//! that need existing data to sync, snapshot or audit.

use crate::Node;
use crate::node::{SEED_DOCTYPE, seed_project_body};

/// Creates `n` `project` documents on `node` as its Administrator and returns their ids, in
/// creation order.
///
/// # Errors
/// The first create call's failure, as the node's API reported it.
pub async fn documents(node: &Node, n: usize) -> Result<Vec<String>, String> {
    let client = node
        .client()
        .ok_or_else(|| format!("node `{}` is not running", node.name()))?;
    let owner = node.admin_id();
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let unique = format!("{}-seed-{i}", node.name());
        let body = seed_project_body(&unique, &owner);
        let reply = client.create(SEED_DOCTYPE, &body).await?;
        reply.expect(201)?;
        ids.push(reply.id()?);
    }
    Ok(ids)
}
