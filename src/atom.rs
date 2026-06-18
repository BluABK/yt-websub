//! Minimal extraction of the few fields we need from YouTube's WebSub Atom
//! payload. The hub sends a fixed, machine-generated format, so namespace-aware
//! substring scanning is robust enough and avoids an XML dependency. Malformed
//! input yields no entries (the caller then appends nothing and returns 2xx).

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub kind: String, // "new" | "updated" | "deleted"
    pub channel_id: String,
    pub video_id: String,
    pub title: String,
    pub ts: String, // updated (preferred) or published; empty for deletions
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = s.find(start)? + start.len();
    let j = s[i..].find(end)? + i;
    Some(&s[i..j])
}

/// Try a namespaced tag first, then the bare tag.
fn tag<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    between(s, &format!("<yt:{}>", name), &format!("</yt:{}>", name))
        .or_else(|| between(s, &format!("<{}>", name), &format!("</{}>", name)))
}

fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&") // must be last
}

/// Find a 24-char `UC...` channel id following any of the known markers.
fn find_channel_id(s: &str) -> Option<String> {
    for marker in ["<yt:channelId>UC", "/channel/UC", "channel_id=UC"] {
        if let Some(pos) = s.find(marker) {
            let start = pos + marker.len() - 2; // include "UC"
            let cand: String = s[start..].chars().take(24).collect();
            if cand.len() == 24
                && cand.starts_with("UC")
                && cand
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Some(cand);
            }
        }
    }
    None
}

pub fn parse(body: &str) -> Vec<Entry> {
    let mut out = Vec::new();

    // Deletions: <at:deleted-entry ref="yt:video:VIDEOID" ...> ... </at:deleted-entry>
    let mut rest = body;
    while let Some(pos) = rest.find("<at:deleted-entry") {
        let block_start = pos;
        let block_end = rest[block_start..]
            .find("</at:deleted-entry>")
            .map(|e| block_start + e + "</at:deleted-entry>".len())
            .unwrap_or(rest.len());
        let block = &rest[block_start..block_end];
        let video_id = between(block, "ref=\"yt:video:", "\"")
            .map(|s| s.to_string())
            .unwrap_or_default();
        if !video_id.is_empty() {
            out.push(Entry {
                kind: "deleted".to_string(),
                channel_id: find_channel_id(block).unwrap_or_default(),
                video_id,
                title: String::new(),
                ts: String::new(),
            });
        }
        rest = &rest[block_end..];
    }

    // Regular entries.
    let mut rest = body;
    while let Some(pos) = rest.find("<entry>") {
        let block_start = pos + "<entry>".len();
        let block_end = rest[block_start..]
            .find("</entry>")
            .map(|e| block_start + e)
            .unwrap_or(rest.len());
        let block = &rest[block_start..block_end];

        let video_id = tag(block, "videoId").map(|s| s.trim().to_string());
        let channel_id = tag(block, "channelId")
            .map(|s| s.trim().to_string())
            .or_else(|| find_channel_id(block));
        if let (Some(video_id), Some(channel_id)) = (video_id, channel_id) {
            if !video_id.is_empty() && !channel_id.is_empty() {
                let title = tag(block, "title")
                    .map(|s| decode_entities(s.trim()))
                    .unwrap_or_default();
                let published = tag(block, "published").map(|s| s.trim().to_string());
                let updated = tag(block, "updated").map(|s| s.trim().to_string());
                let kind = match (&published, &updated) {
                    (Some(p), Some(u)) if p != u => "updated",
                    _ => "new",
                };
                let ts = updated.or(published).unwrap_or_default();
                out.push(Entry {
                    kind: kind.to_string(),
                    channel_id,
                    video_id,
                    title,
                    ts,
                });
            }
        }

        let advance = block_end + "</entry>".len();
        if advance >= rest.len() {
            break;
        }
        rest = &rest[advance..];
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<feed xmlns:yt="http://www.youtube.com/xml/schemas/2015" xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>yt:video:abc123XYZ_-</id>
    <yt:videoId>abc123XYZ_-</yt:videoId>
    <yt:channelId>UCabcdefghijklmnopqrstuv</yt:channelId>
    <title>My Live Stream &amp; chill</title>
    <author><name>Some Channel</name></author>
    <published>2026-06-18T12:00:00+00:00</published>
    <updated>2026-06-18T12:00:00+00:00</updated>
  </entry>
</feed>"#;

    #[test]
    fn parses_single_entry() {
        let e = parse(SAMPLE);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].video_id, "abc123XYZ_-");
        assert_eq!(e[0].channel_id, "UCabcdefghijklmnopqrstuv");
        assert_eq!(e[0].title, "My Live Stream & chill");
        assert_eq!(e[0].kind, "new");
        assert_eq!(e[0].ts, "2026-06-18T12:00:00+00:00");
    }

    #[test]
    fn updated_kind_when_timestamps_differ() {
        let body = SAMPLE.replace(
            "<updated>2026-06-18T12:00:00+00:00</updated>",
            "<updated>2026-06-18T13:30:00+00:00</updated>",
        );
        let e = parse(&body);
        assert_eq!(e[0].kind, "updated");
        assert_eq!(e[0].ts, "2026-06-18T13:30:00+00:00");
    }

    #[test]
    fn parses_deletion() {
        let body = r#"<feed xmlns:at="http://purl.org/atompub/tombstones/1.0">
  <at:deleted-entry ref="yt:video:delVID00000" when="2026-06-18T12:00:00+00:00">
    <link href="https://www.youtube.com/watch?v=delVID00000"/>
    <at:by><name>Chan</name><uri>https://www.youtube.com/channel/UCabcdefghijklmnopqrstuv</uri></at:by>
  </at:deleted-entry>
</feed>"#;
        let e = parse(body);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].kind, "deleted");
        assert_eq!(e[0].video_id, "delVID00000");
        assert_eq!(e[0].channel_id, "UCabcdefghijklmnopqrstuv");
    }

    #[test]
    fn ignores_garbage() {
        assert!(parse("not xml at all").is_empty());
        assert!(parse("<entry><title>no ids</title></entry>").is_empty());
    }
}
