//! Consensus + microdescriptor sync logic with relay-style scheduling.

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use base64ct::Encoding as _;
use rand::Rng;
use tor_checkable::{ExternallySigned, Timebound};
use tor_netdoc::doc::netstatus::{Lifetime, MdConsensus};

use arti_client::TorClient;

/// Fetch consensus, parse it, fetch all microdescs, write everything to disk.
/// Returns the consensus lifetime for scheduling the next sync.
pub async fn sync_once(
    client: &TorClient<tor_rtcompat::PreferredRuntime>,
    output_dir: &Path,
) -> Result<Lifetime> {
    // --- Fetch consensus ---
    tracing::info!("fetching consensus...");
    let consensus_bytes =
        crate::dir::get(client, "/tor/status-vote/current/consensus-microdesc").await?;
    let consensus_text =
        String::from_utf8(consensus_bytes).context("consensus is not valid UTF-8")?;

    // --- Parse consensus ---
    let (_signed, _remainder, unchecked) =
        MdConsensus::parse(&consensus_text).context("parsing consensus")?;
    let consensus = unchecked
        .dangerously_assume_timely()
        .dangerously_assume_wellsigned();

    let lifetime = consensus.lifetime().clone();
    let num_relays = consensus.relays().len();
    tracing::info!(
        "consensus: {} relays, valid_after={}, fresh_until={}, valid_until={}",
        num_relays,
        humantime::format_rfc3339(lifetime.valid_after()),
        humantime::format_rfc3339(lifetime.fresh_until()),
        humantime::format_rfc3339(lifetime.valid_until()),
    );

    // --- Extract microdesc digests ---
    let digests: Vec<_> = consensus
        .relays()
        .iter()
        .map(|rs| *rs.md_digest())
        .collect();

    // --- Fetch microdescs in batches ---
    let batch_size = 500;
    let total_batches = (digests.len() + batch_size - 1) / batch_size;
    let mut all_microdescs = Vec::new();

    for (batch_idx, batch) in digests.chunks(batch_size).enumerate() {
        tracing::info!(
            "fetching microdescs batch {}/{}...",
            batch_idx + 1,
            total_batches,
        );

        let digests_str: Vec<String> = batch
            .iter()
            .map(|d| base64ct::Base64Unpadded::encode_string(d))
            .collect();
        let path = format!("/tor/micro/d/{}", digests_str.join("-"));

        match crate::dir::get(client, &path).await {
            Ok(bytes) => {
                all_microdescs.extend_from_slice(&bytes);
            }
            Err(e) => {
                tracing::warn!("microdesc batch {} failed: {}", batch_idx + 1, e);
            }
        }
    }

    tracing::info!(
        "fetched {} bytes of microdescriptors",
        all_microdescs.len()
    );

    // --- Write files atomically (write to .tmp, then rename) ---
    atomic_write(output_dir, "consensus-microdesc", consensus_text.as_bytes())?;
    tracing::info!(
        "wrote consensus-microdesc ({} bytes)",
        consensus_text.len()
    );

    atomic_write(output_dir, "microdescs", &all_microdescs)?;
    tracing::info!("wrote microdescs ({} bytes)", all_microdescs.len());

    let metadata = serde_json::json!({
        "consensus_flavor": "microdesc",
        "valid_after": humantime::format_rfc3339(lifetime.valid_after()).to_string(),
        "fresh_until": humantime::format_rfc3339(lifetime.fresh_until()).to_string(),
        "valid_until": humantime::format_rfc3339(lifetime.valid_until()).to_string(),
        "num_relays": num_relays,
        "num_microdescs_requested": digests.len(),
        "microdescs_bytes": all_microdescs.len(),
        "synced_at": humantime::format_rfc3339(SystemTime::now()).to_string(),
    });
    atomic_write(
        output_dir,
        "metadata.json",
        serde_json::to_string_pretty(&metadata)?.as_bytes(),
    )?;

    Ok(lifetime)
}

/// Compute the relay-style sync delay: random time in the first half-interval
/// after `fresh_until`.
///
/// Per dir-spec: "the cache downloads a new consensus document at a randomly
/// chosen time in the first half-interval after its current consensus stops
/// being fresh."
pub fn relay_sync_delay(fresh_until: SystemTime, valid_until: SystemTime) -> Duration {
    let half_interval = valid_until
        .duration_since(fresh_until)
        .unwrap_or(Duration::from_secs(1800))
        / 2;
    let offset = rand::rng().random_range(Duration::ZERO..=half_interval);
    let target = fresh_until + offset;
    target
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO)
}

/// Write `data` to `dir/name` atomically via a `.tmp` intermediate.
fn atomic_write(dir: &Path, name: &str, data: &[u8]) -> Result<()> {
    let tmp = dir.join(format!("{}.tmp", name));
    let dst = dir.join(name);
    std::fs::write(&tmp, data).with_context(|| format!("writing {:?}", tmp))?;
    std::fs::rename(&tmp, &dst).with_context(|| format!("renaming to {:?}", dst))?;
    Ok(())
}
