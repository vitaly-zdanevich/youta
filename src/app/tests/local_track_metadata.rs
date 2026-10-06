//! Local track metadata is displayed consistently without inferring or rewriting tags.

use super::*;

/// Checks the complete track line in both normal and caller-labelled descriptions.
fn assert_track_description(item: &LocalMediaItem, expected: Option<&str>) {
    for description in [
        local_media_description(item),
        local_media_description_with_path(item, "~/Music/mock.flac"),
    ] {
        assert_eq!(
            description.lines().find(|line| line.starts_with("Track:")),
            expected,
            "unexpected track metadata in {description:?}"
        );
    }
}

/// Filename prefixes cannot introduce a track before optional metadata is read.
#[test]
fn local_track_metadata_stub_omits_filename_number() {
    let item = local_media_item_stub(PathBuf::from("03 - Mock title.flac"), Some(128));
    assert_eq!(item.title, "03 - Mock title");
    assert!(!item.technical_metadata_probed);
    assert_track_description(&item, None);
}

/// ID3v2 number pairs retain their positions and never promote a total to a track.
#[cfg(feature = "local-metadata")]
#[test]
fn local_track_metadata_id3v2_reads_validated_number_pairs() {
    use lofty::TextEncoding;
    use lofty::id3::v2::{Frame, FrameId, Id3v2Tag, TextInformationFrame};
    use std::borrow::Cow;

    for (value, expected) in [
        ("3", Some("Track: 3")),
        ("03/12", Some("Track: 3/12")),
        (" 03 / 12 ", Some("Track: 3/12")),
        ("3/0", Some("Track: 3")),
        ("", None),
        ("0", None),
        ("0/12", None),
        ("/12", None),
        ("invalid/12", None),
        ("3/invalid", None),
        ("4294967296", None),
    ] {
        let mut tag = Id3v2Tag::new();
        let _ = tag.insert(Frame::Text(TextInformationFrame::new(
            FrameId::Valid(Cow::Borrowed("TRCK")),
            TextEncoding::UTF8,
            value.to_owned(),
        )));
        let mut item = local_media_item_stub(PathBuf::from("mock.mp3"), Some(0));
        apply_local_id3v2_tag(&mut item, &tag);
        assert_track_description(&item, expected);
    }
}

/// ID3v1 stores a track number only; zero and missing values remain absent.
#[cfg(feature = "local-metadata")]
#[test]
fn local_track_metadata_id3v1_reads_number_only() {
    for (number, expected) in [(Some(3), Some("Track: 3")), (Some(0), None), (None, None)] {
        let tag = lofty::id3::v1::Id3v1Tag {
            track_number: number,
            ..lofty::id3::v1::Id3v1Tag::default()
        };
        let mut item = local_media_item_stub(PathBuf::from("mock.mp3"), Some(0));
        apply_local_id3v1_tag(&mut item, &tag);
        assert_track_description(&item, expected);
    }
}

/// Native Vorbis accessors support the standard track and total aliases.
#[cfg(feature = "local-metadata")]
#[test]
fn local_track_metadata_vorbis_reads_number_and_total_aliases() {
    for number_key in ["TRACKNUMBER", "TRACKNUM"] {
        for total_key in ["TRACKTOTAL", "TOTALTRACKS"] {
            let mut tag = lofty::ogg::VorbisComments::new();
            tag.push(number_key.to_owned(), "03".to_owned());
            tag.push(total_key.to_owned(), "12".to_owned());
            let mut item = local_media_item_stub(PathBuf::from("mock.flac"), Some(0));
            apply_local_vorbis_comments(&mut item, &tag);
            assert_track_description(&item, Some("Track: 3/12"));
        }
    }
}

/// Zero, malformed and absent Vorbis values cannot produce misleading track lines.
#[cfg(feature = "local-metadata")]
#[test]
fn local_track_metadata_vorbis_omits_invalid_values() {
    for (number, total, expected) in [
        (Some("3"), None, Some("Track: 3")),
        (Some("3"), Some("0"), Some("Track: 3")),
        (Some("3"), Some("invalid"), Some("Track: 3")),
        (None, Some("12"), None),
        (Some("0"), Some("12"), None),
        (Some("invalid"), Some("12"), None),
        (Some("-3"), None, None),
        (None, None, None),
    ] {
        let mut tag = lofty::ogg::VorbisComments::new();
        if let Some(number) = number {
            tag.push("TRACKNUMBER".to_owned(), number.to_owned());
        }
        if let Some(total) = total {
            tag.push("TRACKTOTAL".to_owned(), total.to_owned());
        }
        let mut item = local_media_item_stub(PathBuf::from("mock.flac"), Some(0));
        apply_local_vorbis_comments(&mut item, &tag);
        assert_track_description(&item, expected);
    }
}

/// Generic metadata preserves separate track fields for other supported containers.
#[cfg(feature = "local-metadata")]
#[test]
fn local_track_metadata_generic_reads_optional_number_and_total() {
    use lofty::tag::{ItemKey, Tag, TagType};

    for (number, total, expected) in [
        (Some("03"), Some("12"), Some("Track: 3/12")),
        (Some("3"), None, Some("Track: 3")),
        (Some("3"), Some("0"), Some("Track: 3")),
        (Some("3"), Some("invalid"), Some("Track: 3")),
        (None, Some("12"), None),
        (Some("0"), Some("12"), None),
        (Some("invalid"), Some("12"), None),
        (Some("4294967296"), None, None),
        (None, None, None),
    ] {
        let mut tag = Tag::new(TagType::Mp4Ilst);
        if let Some(number) = number {
            tag.insert_text(ItemKey::TrackNumber, number.to_owned());
        }
        if let Some(total) = total {
            tag.insert_text(ItemKey::TrackTotal, total.to_owned());
        }
        let mut item = local_media_item_stub(PathBuf::from("mock.m4a"), Some(0));
        apply_local_generic_tag(&mut item, &tag);
        assert_track_description(&item, expected);
    }
}

/// Shared Details places Track after Album and preserves the surrounding metadata.
#[cfg(feature = "local-metadata")]
#[test]
fn local_track_metadata_description_keeps_existing_fields() {
    use lofty::tag::{Accessor, Tag, TagType};

    let mut tag = Tag::new(TagType::VorbisComments);
    tag.set_title("Mock title".to_owned());
    tag.set_artist("Mock artist".to_owned());
    tag.set_album("Mock album".to_owned());
    tag.set_genre("Trip hop".to_owned());
    tag.set_comment("Mock comment".to_owned());
    tag.set_track(3);
    tag.set_track_total(12);
    let mut item = local_media_item_stub(PathBuf::from("mock.flac"), Some(0));
    apply_local_generic_tag(&mut item, &tag);
    assert_eq!(item.title, "Mock title");
    assert!(local_media_description(&item).contains(
		"Artists: Mock artist\nAlbum: Mock album\nTrack: 3/12\nGenre: Trip hop\nComment: Mock comment"
	));
    assert_track_description(&item, Some("Track: 3/12"));
}

/// Builds a metadata-only FLAC fixture with a valid STREAMINFO and raw Vorbis fields.
#[cfg(feature = "local-metadata")]
fn mock_flac_with_track_comments(comments: &[&str]) -> Vec<u8> {
    let mut bytes = b"fLaC".to_vec();
    bytes.extend_from_slice(&[0, 0, 0, 34]);
    let mut stream_info = [0_u8; 34];
    stream_info[0..4].copy_from_slice(&[0, 16, 0, 16]);
    let audio_properties = (44_100_u64 << 44) | (1 << 41) | (15 << 36);
    stream_info[10..18].copy_from_slice(&audio_properties.to_be_bytes());
    bytes.extend_from_slice(&stream_info);
    let mut block = 0_u32.to_le_bytes().to_vec();
    block.extend_from_slice(&u32::try_from(comments.len()).unwrap().to_le_bytes());
    for comment in comments {
        block.extend_from_slice(&u32::try_from(comment.len()).unwrap().to_le_bytes());
        block.extend_from_slice(comment.as_bytes());
    }
    bytes.push(0x84);
    bytes.extend_from_slice(&u32::try_from(block.len()).unwrap().to_be_bytes()[1..]);
    bytes.extend_from_slice(&block);
    bytes
}

/// Lofty's file reader normalizes slash-pair Vorbis fields without rewriting the file.
#[cfg(feature = "local-metadata")]
#[test]
fn local_track_metadata_flac_reader_accepts_slash_pairs_without_writes() {
    let fixture = crate::test_support::canonical_tempdir("local track metadata");
    let path = fixture.path().join("99 - filename.flac");
    let bytes = mock_flac_with_track_comments(&[
        "TITLE=Mock title",
        "ALBUM=Mock album",
        "TRACKNUMBER=03/12",
    ]);
    std::fs::write(&path, &bytes).expect("write mock FLAC metadata");
    let item = local_media_item_without_probe(path.clone());
    assert_eq!(item.title, "Mock title");
    assert_track_description(&item, Some("Track: 3/12"));
    assert_eq!(std::fs::read(path).expect("read original FLAC"), bytes);
}
