//! A small, bounded, read-only view of the Connect state (Smalltify fork commit F2).
//!
//! [`ConnectState`](crate::ConnectState) is internal to the Spirc task. Embedders that cannot use
//! the Web API (for example because of its quota) still need the queue and the remote playback
//! to show what plays next and what plays on another device. `SpircTask` builds a
//! [`ConnectView`] after each event it handles and publishes it through a
//! `tokio::sync::watch` channel ([`Spirc::state`](crate::Spirc::state)) when it changed. A watch
//! channel holds a single value, so a slow reader never queues up old views.
//!
//! Every list and string in the view is bounded, so its size does not depend on the context.

use crate::{
    protocol::{
        connect::Cluster,
        player::{PlayerState, ProvidedTrack},
    },
    state::provider::IsProvider,
};

/// Upcoming tracks kept in [`ConnectView::next`].
pub const MAX_NEXT: usize = 50;
/// Recently played tracks kept in [`ConnectView::previous`].
pub const MAX_PREVIOUS: usize = 20;
/// Longest URI or uid kept, in bytes.
pub const MAX_URI_BYTES: usize = 512;
/// Longest device name kept, in bytes.
pub const MAX_DEVICE_NAME_BYTES: usize = 128;

/// Where a queued track came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueProvider {
    /// Part of the playing context (album, playlist, ...).
    Context,
    /// Added by the user with "add to queue".
    Queue,
    /// Added by Spotify's autoplay after the context ended.
    Autoplay,
    /// Anything else, including tracks marked unavailable.
    Other,
}

/// One track in the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItem {
    /// Spotify URI, e.g. `spotify:track:...`.
    pub uri: String,
    /// Connect uid; distinguishes repeated entries of the same track.
    pub uid: String,
    /// Origin of the entry.
    pub provider: QueueProvider,
}

/// Playback on another device, from the latest cluster update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePlayback {
    /// Name of the active device, `None` when no device is active and this is only the
    /// account's last known playback.
    pub device_name: Option<String>,
    /// URI of the track on that device.
    pub track_uri: String,
    /// Position in milliseconds at `observed_at_ms`.
    pub position_ms: i64,
    /// Whether the remote device is playing (not paused).
    pub playing: bool,
    /// Server timestamp in milliseconds since the Unix epoch at which `position_ms` was valid.
    pub observed_at_ms: i64,
}

/// Bounded snapshot of the Connect state, published by `SpircTask`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectView {
    /// URI of the playing context, if any.
    pub context_uri: Option<String>,
    /// The current track, if any.
    pub current: Option<QueueItem>,
    /// Up to [`MAX_NEXT`] upcoming tracks, next first.
    pub next: Vec<QueueItem>,
    /// Up to [`MAX_PREVIOUS`] played tracks, most recent last.
    pub previous: Vec<QueueItem>,
    /// Shuffle is on.
    pub shuffle: bool,
    /// Repeat context is on.
    pub repeat_context: bool,
    /// Repeat track is on.
    pub repeat_track: bool,
    /// This device is the active Connect device.
    pub active_here: bool,
    /// Playback on another device; always `None` while `active_here`.
    pub remote: Option<RemotePlayback>,
}

impl ConnectView {
    /// Builds the view from this device's player state.
    pub(crate) fn new(
        player: &PlayerState,
        active_here: bool,
        remote: Option<&RemotePlayback>,
    ) -> Self {
        let next = player
            .next_tracks
            .iter()
            .filter_map(QueueItem::from_track)
            .take(MAX_NEXT)
            .collect();

        // prev_tracks are ordered oldest first; keep the most recent ones.
        let mut previous: Vec<QueueItem> = player
            .prev_tracks
            .iter()
            .rev()
            .filter_map(QueueItem::from_track)
            .take(MAX_PREVIOUS)
            .collect();
        previous.reverse();

        let options = player.options.get_or_default();
        Self {
            context_uri: non_empty(&player.context_uri).map(|uri| bounded(uri, MAX_URI_BYTES)),
            current: player.track.as_ref().and_then(QueueItem::from_track),
            next,
            previous,
            shuffle: options.shuffling_context,
            repeat_context: options.repeating_context,
            repeat_track: options.repeating_track,
            active_here,
            remote: if active_here { None } else { remote.cloned() },
        }
    }
}

impl QueueItem {
    /// `None` for empty entries and the hidden delimiter tracks Connect uses between pages.
    fn from_track(track: &ProvidedTrack) -> Option<Self> {
        if track.uri.is_empty() || track.uri == "spotify:delimiter" {
            return None;
        }
        let provider = if track.is_context() {
            QueueProvider::Context
        } else if track.is_queue() {
            QueueProvider::Queue
        } else if track.is_autoplay() {
            QueueProvider::Autoplay
        } else {
            QueueProvider::Other
        };
        Some(Self {
            uri: bounded(&track.uri, MAX_URI_BYTES),
            uid: bounded(&track.uid, MAX_URI_BYTES),
            provider,
        })
    }
}

impl RemotePlayback {
    /// Remote playback described by a cluster, or `None` when this device is active or the
    /// cluster names no track.
    pub(crate) fn from_cluster(cluster: &Cluster, own_device_id: &str) -> Option<Self> {
        if cluster.active_device_id == own_device_id {
            return None;
        }
        let player = cluster.player_state.as_ref()?;
        let track_uri = non_empty(&player.track.as_ref()?.uri)?;
        let device_name = non_empty(&cluster.active_device_id).map(|id| {
            let name = cluster
                .device
                .get(id)
                .map(|d| d.name.as_str())
                .unwrap_or("");
            bounded(name, MAX_DEVICE_NAME_BYTES)
        });
        Some(Self {
            playing: device_name.is_some() && player.is_playing && !player.is_paused,
            device_name,
            track_uri: bounded(track_uri, MAX_URI_BYTES),
            position_ms: player.position_as_of_timestamp,
            observed_at_ms: player.timestamp,
        })
    }
}

fn non_empty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

/// Copies at most `max` bytes of `s`, cut at a character boundary.
fn bounded(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::connect::DeviceInfo;

    fn track(uri: &str, provider: &str) -> ProvidedTrack {
        ProvidedTrack {
            uri: uri.into(),
            uid: format!("uid-{uri}"),
            provider: provider.into(),
            ..Default::default()
        }
    }

    fn player(next: usize, prev: usize) -> PlayerState {
        let mut player = PlayerState {
            context_uri: "spotify:album:a".into(),
            ..Default::default()
        };
        player.track = Some(track("spotify:track:current", "context")).into();
        player.next_tracks = (0..next)
            .map(|i| track(&format!("spotify:track:n{i}"), "context"))
            .collect();
        player.prev_tracks = (0..prev)
            .map(|i| track(&format!("spotify:track:p{i}"), "context"))
            .collect();
        player
    }

    fn remote_cluster(active: &str) -> Cluster {
        let mut cluster = Cluster {
            active_device_id: active.into(),
            ..Default::default()
        };
        cluster.device.insert(
            "laptop".into(),
            DeviceInfo {
                name: "Laptop".into(),
                ..Default::default()
            },
        );
        let state = cluster.player_state.mut_or_insert_default();
        state.track = Some(track("spotify:track:remote", "context")).into();
        state.position_as_of_timestamp = 61_000;
        state.timestamp = 1_700_000_000_000;
        state.is_playing = true;
        cluster
    }

    #[test]
    fn lists_are_bounded_and_keep_the_nearest_tracks() {
        let view = ConnectView::new(&player(80, 30), true, None);
        assert_eq!(view.next.len(), MAX_NEXT);
        assert_eq!(view.next[0].uri, "spotify:track:n0");
        assert_eq!(view.next[MAX_NEXT - 1].uri, "spotify:track:n49");
        assert_eq!(view.previous.len(), MAX_PREVIOUS);
        assert_eq!(view.previous[0].uri, "spotify:track:p10");
        assert_eq!(view.previous[MAX_PREVIOUS - 1].uri, "spotify:track:p29");
        assert_eq!(view.current.unwrap().uri, "spotify:track:current");
        assert_eq!(view.context_uri.as_deref(), Some("spotify:album:a"));
    }

    #[test]
    fn delimiters_are_skipped_and_providers_mapped() {
        let mut p = player(0, 0);
        p.next_tracks = vec![
            track("spotify:track:q", "queue"),
            track("spotify:delimiter", "context"),
            track("spotify:track:a", "autoplay"),
            track("spotify:track:u", "unavailable"),
        ];
        let view = ConnectView::new(&p, true, None);
        let providers: Vec<_> = view.next.iter().map(|i| i.provider).collect();
        assert_eq!(
            providers,
            [
                QueueProvider::Queue,
                QueueProvider::Autoplay,
                QueueProvider::Other
            ]
        );
    }

    #[test]
    fn strings_are_cut_at_char_boundaries() {
        let long = "é".repeat(400); // 800 bytes
        let cut = bounded(&long, MAX_URI_BYTES);
        assert_eq!(cut.len(), MAX_URI_BYTES);
        assert!(cut.chars().all(|c| c == 'é'));
        assert_eq!(bounded("abc", 2), "ab");
        assert_eq!(bounded("é", 1), "");
    }

    #[test]
    fn takeover_by_another_device_is_reported_as_remote_playback() {
        let remote = RemotePlayback::from_cluster(&remote_cluster("laptop"), "phone").unwrap();
        assert_eq!(remote.device_name.as_deref(), Some("Laptop"));
        assert_eq!(remote.track_uri, "spotify:track:remote");
        assert_eq!(remote.position_ms, 61_000);
        assert_eq!(remote.observed_at_ms, 1_700_000_000_000);
        assert!(remote.playing);

        let view = ConnectView::new(&PlayerState::default(), false, Some(&remote));
        assert!(!view.active_here);
        assert_eq!(view.remote, Some(remote.clone()));

        // Once playback is here again, remote playback is hidden.
        let view = ConnectView::new(&PlayerState::default(), true, Some(&remote));
        assert_eq!(view.remote, None);
    }

    #[test]
    fn own_or_empty_clusters_give_no_remote_device() {
        assert_eq!(
            RemotePlayback::from_cluster(&remote_cluster("phone"), "phone"),
            None
        );
        assert_eq!(
            RemotePlayback::from_cluster(&Cluster::default(), "phone"),
            None
        );

        // No active device: the account's last playback, without a device and not playing.
        let last = RemotePlayback::from_cluster(&remote_cluster(""), "phone").unwrap();
        assert_eq!(last.device_name, None);
        assert!(!last.playing);
        assert_eq!(last.track_uri, "spotify:track:remote");
    }
}
