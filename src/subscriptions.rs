//! Portable OPML subscription trees.
//!
//! Youta uses OPML rather than a private subscription format so podcast and
//! `YouTube` channel subscriptions can be moved to and from other readers by
//! copying one file. Folder outlines are represented recursively and feed,
//! website, and `YouTube` URLs survive import/export round trips.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use opml::{OPML, Outline};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::config::{Config, ConfigError};

/// A complete portable subscription document.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubscriptionTree {
    /// Top-level folders and subscriptions in display order.
    pub items: Vec<SubscriptionNode>,
}

impl SubscriptionTree {
    /// Parses an OPML document.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed XML or an invalid subscription URL.
    pub fn from_opml(xml: &str) -> Result<Self, SubscriptionError> {
        match OPML::from_str(xml) {
            Ok(document) => {
                let items = document
                    .body
                    .outlines
                    .into_iter()
                    .map(node_from_outline)
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Self { items })
            }
            Err(opml::Error::BodyHasNoOutlines) => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    /// Reads and parses OPML from any reader.
    ///
    /// # Errors
    ///
    /// Returns an error when input cannot be read or parsed.
    pub fn from_reader(reader: &mut impl Read) -> Result<Self, SubscriptionError> {
        let mut xml = String::new();
        reader.read_to_string(&mut xml)?;
        Self::from_opml(&xml)
    }

    /// Serializes the complete nested tree as OPML 2.0.
    ///
    /// # Errors
    ///
    /// Returns an error if the tree cannot be represented as OPML.
    pub fn to_opml(&self) -> Result<String, SubscriptionError> {
        let mut document = OPML::default();
        if let Some(head) = document.head.as_mut() {
            head.title = Some("Youta subscriptions".to_owned());
        }
        document.body.outlines = self.items.iter().map(outline_from_node).collect();
        Ok(document.to_string()?)
    }

    /// Writes OPML to any writer.
    ///
    /// # Errors
    ///
    /// Returns an error when serialization or output fails.
    pub fn to_writer(&self, writer: &mut impl Write) -> Result<(), SubscriptionError> {
        writer.write_all(self.to_opml()?.as_bytes())?;
        Ok(())
    }

    /// Counts all subscriptions recursively.
    #[must_use]
    pub fn subscription_count(&self) -> usize {
        self.items
            .iter()
            .map(SubscriptionNode::subscription_count)
            .sum()
    }

    /// Returns selectable subscriptions in stable depth-first OPML order.
    ///
    /// Folder nodes are represented by the `depth` carried by each returned
    /// subscription rather than becoming non-playable list rows.
    #[must_use]
    pub fn flattened_subscriptions(&self) -> Vec<FlattenedSubscription> {
        let mut flattened = Vec::with_capacity(self.subscription_count());
        flatten_subscriptions(&self.items, 0, &mut flattened);
        flattened
    }

    /// Returns whether the tree contains a subscription for a `YouTube` channel.
    ///
    /// Both the standard uploads-feed URL and the canonical channel website
    /// are recognized so subscriptions imported from another OPML reader do
    /// not become duplicates.
    #[must_use]
    pub fn contains_youtube_channel(&self, channel_id: &str) -> bool {
        !channel_id.is_empty()
            && self
                .items
                .iter()
                .any(|node| node_contains_youtube_channel(node, channel_id))
    }

    /// Adds a top-level portable subscription for a `YouTube` channel.
    ///
    /// The operation is idempotent: an imported subscription with the same
    /// channel ID is retained instead of adding another outline.
    ///
    /// Returns `true` when a new subscription was added.
    pub fn subscribe_youtube_channel(
        &mut self,
        title: impl Into<String>,
        channel_id: &str,
    ) -> bool {
        if channel_id.is_empty() || self.contains_youtube_channel(channel_id) {
            return false;
        }

        let Ok(mut feed_url) = Url::parse("https://www.youtube.com/feeds/videos.xml") else {
            return false;
        };
        feed_url
            .query_pairs_mut()
            .append_pair("channel_id", channel_id);
        let Ok(mut website_url) = Url::parse("https://www.youtube.com") else {
            return false;
        };
        {
            let Ok(mut segments) = website_url.path_segments_mut() else {
                return false;
            };
            segments.extend(["channel", channel_id]);
        }

        let mut subscription = Subscription::new(title, feed_url);
        subscription.website_url = Some(website_url);
        self.items
            .push(SubscriptionNode::Subscription(subscription));
        true
    }

    /// Removes every subscription matching a `YouTube` channel ID.
    ///
    /// Empty user-created folders are preserved. Returns `true` when at least
    /// one matching subscription was removed.
    pub fn unsubscribe_youtube_channel(&mut self, channel_id: &str) -> bool {
        if channel_id.is_empty() {
            return false;
        }
        remove_youtube_channel(&mut self.items, channel_id)
    }
}

fn node_contains_youtube_channel(node: &SubscriptionNode, channel_id: &str) -> bool {
    match node {
        SubscriptionNode::Folder(folder) => folder
            .children
            .iter()
            .any(|child| node_contains_youtube_channel(child, channel_id)),
        SubscriptionNode::Subscription(subscription) => {
            subscription_matches_youtube_channel(subscription, channel_id)
        }
    }
}

fn remove_youtube_channel(nodes: &mut Vec<SubscriptionNode>, channel_id: &str) -> bool {
    let original_len = nodes.len();
    nodes.retain(|node| {
        !matches!(
            node,
            SubscriptionNode::Subscription(subscription)
                if subscription_matches_youtube_channel(subscription, channel_id)
        )
    });
    let mut removed = nodes.len() != original_len;
    for node in nodes {
        if let SubscriptionNode::Folder(folder) = node {
            removed |= remove_youtube_channel(&mut folder.children, channel_id);
        }
    }
    removed
}

fn subscription_matches_youtube_channel(subscription: &Subscription, channel_id: &str) -> bool {
    subscription.kind == SubscriptionKind::YouTube
        && std::iter::once(&subscription.url)
            .chain(subscription.website_url.iter())
            .any(|url| url_matches_youtube_channel(url, channel_id))
}

fn url_matches_youtube_channel(url: &Url, channel_id: &str) -> bool {
    youtube_channel_id_from_url(url).as_deref() == Some(channel_id)
}

fn youtube_channel_id_from_url(url: &Url) -> Option<String> {
    if !url
        .host_str()
        .is_some_and(|host| host == "youtube.com" || host.ends_with(".youtube.com"))
    {
        return None;
    }
    if url.path() == "/feeds/videos.xml" && url.fragment().is_none() {
        let mut channel_ids = url
            .query_pairs()
            .filter_map(|(name, candidate)| (name == "channel_id").then(|| candidate.into_owned()));
        if let Some(channel_id) = channel_ids.next()
            && channel_ids.next().is_none()
            && valid_youtube_channel_id(&channel_id)
        {
            return Some(channel_id);
        }
    }
    let mut segments = url.path_segments()?;
    if segments.next() != Some("channel") {
        return None;
    }
    let channel_id = segments.next()?;
    (segments.next().is_none() && url.query().is_none() && url.fragment().is_none())
        .then_some(channel_id)
        .filter(|candidate| valid_youtube_channel_id(candidate))
        .map(ToOwned::to_owned)
}

fn valid_youtube_channel_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// One outline in a subscription tree.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum SubscriptionNode {
    /// A named, recursively nested folder.
    Folder(SubscriptionFolder),
    /// A `YouTube` channel, podcast feed, or portable link.
    Subscription(Subscription),
}

impl SubscriptionNode {
    /// Counts subscriptions below this node.
    #[must_use]
    pub fn subscription_count(&self) -> usize {
        match self {
            Self::Folder(folder) => folder.children.iter().map(Self::subscription_count).sum(),
            Self::Subscription(_) => 1,
        }
    }
}

/// A folder whose children retain their OPML display order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubscriptionFolder {
    /// User-visible folder name.
    pub title: String,
    /// Nested folders and subscriptions.
    pub children: Vec<SubscriptionNode>,
}

/// A portable source subscription.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Subscription {
    /// User-visible channel, podcast, or source name.
    pub title: String,
    /// Primary feed or source URL.
    pub url: Url,
    /// Optional human-facing channel or podcast page.
    pub website_url: Option<Url>,
    /// Optional public source description from OPML.
    pub description: Option<String>,
    /// Source classification inferred during import or selected on creation.
    pub kind: SubscriptionKind,
}

impl Subscription {
    /// Creates a subscription and infers its kind from the supplied URL.
    #[must_use]
    pub fn new(title: impl Into<String>, url: Url) -> Self {
        let kind = classify_url(&url, None);
        Self {
            title: title.into(),
            url,
            website_url: None,
            description: None,
            kind,
        }
    }

    /// Returns the exact `YouTube` channel identifier represented by this
    /// subscription, when one is present.
    ///
    /// Standard uploads-feed `channel_id` parameters and canonical
    /// `/channel/<id>` website paths are recognized. Handle-only links such as
    /// `/@example` intentionally return `None`; resolving handles requires a
    /// provider operation rather than guessing an identifier.
    #[must_use]
    pub fn youtube_channel_id(&self) -> Option<String> {
        if self.kind != SubscriptionKind::YouTube {
            return None;
        }
        std::iter::once(&self.url)
            .chain(self.website_url.iter())
            .find_map(youtube_channel_id_from_url)
    }
}

/// One selectable subscription with its original OPML folder depth.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlattenedSubscription {
    /// Number of parent folders surrounding the subscription.
    pub depth: usize,
    /// Complete portable subscription record.
    pub subscription: Subscription,
}

fn flatten_subscriptions(
    nodes: &[SubscriptionNode],
    depth: usize,
    flattened: &mut Vec<FlattenedSubscription>,
) {
    for node in nodes {
        match node {
            SubscriptionNode::Folder(folder) => {
                flatten_subscriptions(&folder.children, depth.saturating_add(1), flattened);
            }
            SubscriptionNode::Subscription(subscription) => {
                flattened.push(FlattenedSubscription {
                    depth,
                    subscription: subscription.clone(),
                });
            }
        }
    }
}

/// Portable subscription source categories.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubscriptionKind {
    /// A `YouTube` channel, playlist, or channel feed.
    YouTube,
    /// An RSS or Atom podcast/feed.
    Rss,
    /// A link using a format Youta does not classify yet.
    Other,
}

/// Loads the configured OPML file, returning an empty tree when it is absent.
///
/// # Errors
///
/// Returns an error when the file cannot be read or parsed.
pub fn load(config: &Config) -> Result<SubscriptionTree, SubscriptionError> {
    let path = config.subscriptions_file();
    if !path.exists() {
        return Ok(SubscriptionTree::default());
    }
    let mut file = File::open(path)?;
    SubscriptionTree::from_reader(&mut file)
}

/// Saves subscriptions to the configured OPML file.
///
/// The temporary and final file both remain inside Youta's application
/// directory. On Unix the resulting file is mode `0600`.
///
/// # Errors
///
/// Returns an error when the application directory cannot be prepared or the
/// OPML file cannot be serialized, secured, or written.
pub fn save(config: &Config, tree: &SubscriptionTree) -> Result<(), SubscriptionError> {
    config.ensure_directories()?;
    let path = config.subscriptions_file();
    let temporary_path = path.with_extension("opml.tmp");
    {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary_path)?;
        set_private_file_permissions(&temporary_path)?;
        tree.to_writer(&mut file)?;
        file.sync_all()?;
    }
    fs::rename(&temporary_path, &path)?;
    Ok(())
}

/// Errors produced by subscription import or export.
#[derive(Debug, thiserror::Error)]
pub enum SubscriptionError {
    /// OPML XML was malformed or unsupported.
    #[error("invalid OPML subscription document: {0}")]
    Opml(#[from] opml::Error),
    /// A file could not be read or written.
    #[error("subscription file operation failed: {0}")]
    Io(#[from] std::io::Error),
    /// The application directory could not be prepared.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// An outline URL was not a valid absolute URL.
    #[error("invalid subscription URL {value:?}: {source}")]
    InvalidUrl {
        /// Original OPML attribute value.
        value: String,
        /// URL parser error.
        source: url::ParseError,
    },
}

fn node_from_outline(outline: Outline) -> Result<SubscriptionNode, SubscriptionError> {
    let title = outline
        .title
        .clone()
        .unwrap_or_else(|| outline.text.clone());
    let primary_url = outline
        .xml_url
        .as_deref()
        .or(outline.url.as_deref())
        .or(outline.html_url.as_deref());

    let Some(primary_url) = primary_url else {
        let children = outline
            .outlines
            .into_iter()
            .map(node_from_outline)
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(SubscriptionNode::Folder(SubscriptionFolder {
            title,
            children,
        }));
    };

    let url = parse_url(primary_url)?;
    let website_url = outline
        .html_url
        .as_deref()
        .filter(|value| *value != primary_url)
        .map(parse_url)
        .transpose()?;
    let kind = classify_url(&url, outline.r#type.as_deref());
    Ok(SubscriptionNode::Subscription(Subscription {
        title,
        url,
        website_url,
        description: outline.description,
        kind,
    }))
}

fn outline_from_node(node: &SubscriptionNode) -> Outline {
    match node {
        SubscriptionNode::Folder(folder) => Outline {
            text: folder.title.clone(),
            title: Some(folder.title.clone()),
            outlines: folder.children.iter().map(outline_from_node).collect(),
            ..Outline::default()
        },
        SubscriptionNode::Subscription(subscription) => {
            let (outline_type, xml_url, url) = match subscription.kind {
                SubscriptionKind::YouTube | SubscriptionKind::Rss => (
                    Some("rss".to_owned()),
                    Some(subscription.url.as_str().to_owned()),
                    None,
                ),
                SubscriptionKind::Other => (
                    Some("link".to_owned()),
                    None,
                    Some(subscription.url.as_str().to_owned()),
                ),
            };
            Outline {
                text: subscription.title.clone(),
                title: Some(subscription.title.clone()),
                r#type: outline_type,
                xml_url,
                html_url: subscription
                    .website_url
                    .as_ref()
                    .map(|url| url.as_str().to_owned()),
                url,
                description: subscription.description.clone(),
                ..Outline::default()
            }
        }
    }
}

fn classify_url(url: &Url, outline_type: Option<&str>) -> SubscriptionKind {
    let host = url.host_str().unwrap_or_default();
    if host == "youtu.be" || host == "youtube.com" || host.ends_with(".youtube.com") {
        return SubscriptionKind::YouTube;
    }
    if outline_type.is_some_and(|kind| {
        kind.eq_ignore_ascii_case("rss")
            || kind.eq_ignore_ascii_case("atom")
            || kind.eq_ignore_ascii_case("feed")
    }) {
        return SubscriptionKind::Rss;
    }
    let feed_extension = Path::new(url.path())
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("rss") || extension.eq_ignore_ascii_case("xml")
        });
    if matches!(url.scheme(), "http" | "https") && (feed_extension || url.path().contains("/feed"))
    {
        return SubscriptionKind::Rss;
    }
    SubscriptionKind::Other
}

fn parse_url(value: &str) -> Result<Url, SubscriptionError> {
    Url::parse(value).map_err(|source| SubscriptionError::InvalidUrl {
        value: value.to_owned(),
        source,
    })
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn parse(value: &str) -> Url {
        Url::parse(value).expect("valid test URL")
    }

    fn sample_tree() -> SubscriptionTree {
        SubscriptionTree {
            items: vec![SubscriptionNode::Folder(SubscriptionFolder {
                title: "Health".to_owned(),
                children: vec![
                    SubscriptionNode::Subscription(Subscription {
                        title: "Medical channel".to_owned(),
                        url: parse("https://www.youtube.com/feeds/videos.xml?channel_id=UCexample"),
                        website_url: Some(parse("https://www.youtube.com/@medical")),
                        description: Some("Evidence-based videos".to_owned()),
                        kind: SubscriptionKind::YouTube,
                    }),
                    SubscriptionNode::Folder(SubscriptionFolder {
                        title: "Podcasts".to_owned(),
                        children: vec![SubscriptionNode::Subscription(Subscription {
                            title: "Example podcast".to_owned(),
                            url: parse("https://example.org/feed.xml"),
                            website_url: Some(parse("https://example.org/podcast")),
                            description: None,
                            kind: SubscriptionKind::Rss,
                        })],
                    }),
                ],
            })],
        }
    }

    #[test]
    fn nested_youtube_and_rss_round_trip() {
        let tree = sample_tree();
        let xml = tree.to_opml().expect("serialize OPML");
        let restored = SubscriptionTree::from_opml(&xml).expect("parse OPML");
        assert_eq!(restored, tree);
        assert_eq!(restored.subscription_count(), 2);
        assert!(xml.contains("channel_id=UCexample"));
        assert!(xml.contains("https://example.org/feed.xml"));
    }

    #[test]
    fn flattened_subscriptions_keep_depth_first_source_identity() {
        let flattened = sample_tree().flattened_subscriptions();

        assert_eq!(flattened.len(), 2);
        assert_eq!(flattened[0].depth, 1);
        assert_eq!(flattened[0].subscription.title, "Medical channel");
        assert_eq!(
            flattened[0].subscription.youtube_channel_id().as_deref(),
            Some("UCexample")
        );
        assert_eq!(flattened[1].depth, 2);
        assert_eq!(flattened[1].subscription.title, "Example podcast");
        assert!(flattened[1].subscription.youtube_channel_id().is_none());
    }

    #[test]
    fn youtube_channel_id_requires_an_exact_feed_or_channel_url() {
        let fixtures = [
            (
                "https://www.youtube.com/feeds/videos.xml?channel_id=UC_feed-123",
                None,
                Some("UC_feed-123"),
            ),
            (
                "https://www.youtube.com/@fixture",
                Some("https://www.youtube.com/channel/UC_website-456"),
                Some("UC_website-456"),
            ),
            ("https://www.youtube.com/@handle-only", None, None),
            (
                "https://notyoutube.example/feeds/videos.xml?channel_id=UClookalike",
                None,
                None,
            ),
            (
                "https://www.youtube.com/feeds/videos.xml?channel_id=bad%2Fpath",
                None,
                None,
            ),
            (
                "https://www.youtube.com/watch?v=fixture&channel_id=UCwrongpath",
                None,
                None,
            ),
            (
                "https://www.youtube.com/feeds/videos.xml?channel_id=UCone&channel_id=UCtwo",
                None,
                None,
            ),
            (
                "https://www.youtube.com/channel/UCfixture/videos",
                None,
                None,
            ),
        ];

        for (source, website, expected) in fixtures {
            let mut subscription = Subscription::new("Fixture", parse(source));
            subscription.kind = SubscriptionKind::YouTube;
            subscription.website_url = website.map(parse);
            assert_eq!(
                subscription.youtube_channel_id().as_deref(),
                expected,
                "source {source}"
            );
        }
    }

    #[test]
    fn youtube_channel_mutations_are_idempotent_and_portable() {
        let mut tree = SubscriptionTree::default();

        assert!(tree.subscribe_youtube_channel("Fixture channel", "UCfixture"));
        assert!(!tree.subscribe_youtube_channel("Duplicate title", "UCfixture"));
        assert!(tree.contains_youtube_channel("UCfixture"));
        assert_eq!(tree.subscription_count(), 1);

        let xml = tree.to_opml().expect("serialize YouTube subscription");
        assert!(xml.contains("channel_id=UCfixture"));
        assert!(xml.contains("https://www.youtube.com/channel/UCfixture"));

        let mut restored = SubscriptionTree::from_opml(&xml).expect("restore YouTube subscription");
        assert!(restored.unsubscribe_youtube_channel("UCfixture"));
        assert!(!restored.unsubscribe_youtube_channel("UCfixture"));
        assert!(!restored.contains_youtube_channel("UCfixture"));
        assert_eq!(restored.subscription_count(), 0);
    }

    #[test]
    fn imported_nested_channel_website_is_recognized_and_removed() {
        let mut tree = SubscriptionTree {
            items: vec![SubscriptionNode::Folder(SubscriptionFolder {
                title: "Imported".to_owned(),
                children: vec![SubscriptionNode::Subscription(Subscription {
                    title: "Fixture channel".to_owned(),
                    url: parse("https://www.youtube.com/@fixture"),
                    website_url: Some(parse("https://www.youtube.com/channel/UCnestedfixture")),
                    description: None,
                    kind: SubscriptionKind::YouTube,
                })],
            })],
        };

        assert!(tree.contains_youtube_channel("UCnestedfixture"));
        assert!(!tree.subscribe_youtube_channel("Duplicate", "UCnestedfixture"));
        assert!(tree.unsubscribe_youtube_channel("UCnestedfixture"));
        let SubscriptionNode::Folder(folder) = &tree.items[0] else {
            panic!("expected imported folder");
        };
        assert!(folder.children.is_empty(), "user folder must be preserved");
    }

    #[test]
    fn imports_common_opml_without_title_attributes() {
        let xml = r#"
		<opml version="2.0">
			<head/>
			<body>
				<outline text="Channels">
					<outline text="A channel" type="rss"
						xmlUrl="https://www.youtube.com/feeds/videos.xml?channel_id=UC123"
						htmlUrl="https://www.youtube.com/channel/UC123"/>
				</outline>
			</body>
		</opml>
		"#;
        let tree = SubscriptionTree::from_opml(xml).expect("parse OPML");
        assert_eq!(tree.subscription_count(), 1);
        let SubscriptionNode::Folder(folder) = &tree.items[0] else {
            panic!("expected folder");
        };
        let SubscriptionNode::Subscription(subscription) = &folder.children[0] else {
            panic!("expected subscription");
        };
        assert_eq!(subscription.title, "A channel");
        assert_eq!(subscription.kind, SubscriptionKind::YouTube);
        assert_eq!(
            subscription.website_url.as_ref().map(Url::as_str),
            Some("https://www.youtube.com/channel/UC123")
        );
    }

    #[test]
    fn empty_documents_are_supported() {
        let tree = SubscriptionTree::default();
        let xml = tree.to_opml().expect("serialize empty OPML");
        assert_eq!(
            SubscriptionTree::from_opml(&xml).expect("parse empty OPML"),
            tree
        );
    }

    #[test]
    fn load_and_save_stay_in_config_directory() {
        let directory = tempdir().expect("temporary directory");
        let config = Config::for_dir(directory.path().join("youta"));
        let tree = sample_tree();
        save(&config, &tree).expect("save subscriptions");
        assert_eq!(load(&config).expect("load subscriptions"), tree);
        assert!(config.subscriptions_file().starts_with(config.config_dir()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(config.subscriptions_file())
                .expect("subscription metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn invalid_urls_report_the_original_value() {
        let xml = r#"
		<opml version="2.0"><head/><body>
			<outline text="Broken" type="rss" xmlUrl="not a URL"/>
		</body></opml>
		"#;
        let error = SubscriptionTree::from_opml(xml).expect_err("invalid URL");
        assert!(matches!(
            error,
            SubscriptionError::InvalidUrl { value, .. } if value == "not a URL"
        ));
    }
}
