//! hitomi.la CDN access: the nozomi ID index, gallery metadata, liveness.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::{Client, StatusCode};
use serde_json::Value;

const CDN: &str = "https://ltn.gold-usergeneratedcontent.net";
const THUMB_CDN: &str = "https://atn.gold-usergeneratedcontent.net";

/// Small requests are bounded by connect/idle time plus a total cap; the
/// multi-MB nozomi download overrides the total cap (slow but steady CDN
/// paths from CI runners took over 30 s).
pub fn client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36")
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(60))
        .build()?)
}

/// Every gallery ID hitomi currently lists (`index-all.nozomi`: big-endian u32s).
pub async fn nozomi_ids(client: &Client) -> Result<HashSet<i64>> {
    let mut last_err = None;
    for attempt in 1..=3 {
        let started = std::time::Instant::now();
        match nozomi_once(client).await {
            Ok(bytes) => {
                eprintln!("nozomi: {} bytes in {:.1?}", bytes.len(), started.elapsed());
                let (ids, _) = bytes.as_chunks::<4>();
                return Ok(ids.iter().map(|&c| u32::from_be_bytes(c) as i64).collect());
            }
            Err(e) => {
                eprintln!(
                    "nozomi attempt {attempt} failed after {:.1?}: {e:#}",
                    started.elapsed()
                );
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap())
}

async fn nozomi_once(client: &Client) -> Result<Vec<u8>> {
    let resp = client
        .get(format!("{CDN}/index-all.nozomi"))
        .header("Referer", "https://hitomi.la/")
        .header("Origin", "https://hitomi.la")
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .context("nozomi request")?;
    if !resp.status().is_success() {
        bail!("nozomi returned {}", resp.status());
    }
    Ok(resp.bytes().await.context("nozomi body")?.to_vec())
}

/// Gallery metadata in the dataset's column layout (multi-value fields as
/// JSON arrays, the same encoding the app writes).
#[derive(Debug, PartialEq)]
pub struct Gallery {
    pub id: i64,
    pub title: String,
    pub gallery_type: String,
    pub language: Option<String>,
    pub artists: String,
    pub groups: String,
    pub series: String,
    pub characters: String,
    pub tags: String,
}

enum Js {
    Found(Value),
    Missing,
    Failed,
}

/// `galleries/{id}.js`: a `var galleryinfo = {...}` script.
async fn gallery_js(client: &Client, id: i64) -> Js {
    let Ok(resp) = client
        .get(format!("{CDN}/galleries/{id}.js"))
        .header("Referer", format!("https://hitomi.la/reader/{id}.html"))
        .send()
        .await
    else {
        return Js::Failed;
    };
    match resp.status() {
        StatusCode::NOT_FOUND => return Js::Missing,
        s if !s.is_success() => return Js::Failed,
        _ => {}
    }
    let Ok(text) = resp.text().await else {
        return Js::Failed;
    };
    let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) else {
        return Js::Failed;
    };
    serde_json::from_str(&text[start..=end]).map_or(Js::Failed, Js::Found)
}

/// Metadata of a gallery; `None` when it could not be fetched.
pub async fn fetch_gallery(client: &Client, id: i64) -> Option<Gallery> {
    match gallery_js(client, id).await {
        Js::Found(data) => Some(compact(id, &data)),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// Metadata and the first image's thumbnail are served.
    Alive,
    /// Metadata 404, no files, or thumbnail 404 — the images are gone too.
    Dead,
    /// Network error or unexpected status; check again later.
    Unknown,
}

/// Whether a gallery can still be downloaded. A thumbnail 404 matched an
/// original-image 404 in every sampled case, and `blocked`/forbidden
/// galleries still serve images, so neither nozomi nor `blocked` decides this.
pub async fn verify(client: &Client, id: i64) -> Verdict {
    let mut verdict = Verdict::Unknown;
    for _ in 0..2 {
        verdict = verify_once(client, id).await;
        if verdict != Verdict::Unknown {
            break;
        }
    }
    verdict
}

async fn verify_once(client: &Client, id: i64) -> Verdict {
    let data = match gallery_js(client, id).await {
        Js::Found(data) => data,
        Js::Missing => return Verdict::Dead,
        Js::Failed => return Verdict::Unknown,
    };
    let Some(url) = data["files"][0]["hash"].as_str().and_then(thumbnail_url) else {
        return Verdict::Dead;
    };
    match client
        .head(url)
        .header("Referer", "https://hitomi.la/")
        .send()
        .await
        .map(|r| r.status())
    {
        Ok(s) if s.is_success() => Verdict::Alive,
        Ok(StatusCode::NOT_FOUND) => Verdict::Dead,
        _ => Verdict::Unknown,
    }
}

/// `webpbigtn/{hash[-1]}/{hash[-3..-1]}/{hash}.webp`
fn thumbnail_url(hash: &str) -> Option<String> {
    let n = hash.len();
    (n >= 3 && hash.is_ascii()).then(|| {
        format!(
            "{THUMB_CDN}/webpbigtn/{}/{}/{hash}.webp",
            &hash[n - 1..],
            &hash[n - 3..n - 1]
        )
    })
}

fn compact(id: i64, data: &Value) -> Gallery {
    let json = |v: Vec<String>| serde_json::to_string(&v).unwrap_or_else(|_| "[]".into());
    Gallery {
        id,
        title: data["title"].as_str().unwrap_or("").to_string(),
        gallery_type: data["type"].as_str().unwrap_or("").to_string(),
        language: data["language"].as_str().map(String::from),
        artists: json(names(data, "artists", "artist")),
        groups: json(names(data, "groups", "group")),
        series: json(names(data, "parodys", "parody")),
        characters: json(names(data, "characters", "character")),
        tags: json(sorted_tags(data)),
    }
}

/// `data[list_key][*][item_key]` strings, e.g. `artists[*].artist`.
fn names(data: &Value, list_key: &str, item_key: &str) -> Vec<String> {
    data[list_key]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v[item_key].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Plain tags sorted, then `female:`/`male:` prefixed tags sorted.
fn sorted_tags(data: &Value) -> Vec<String> {
    let flag = |v: &Value| v.as_i64().unwrap_or(0) != 0 || v.as_str() == Some("1");
    let (mut plain, mut prefixed) = (Vec::new(), Vec::new());
    for item in data["tags"].as_array().into_iter().flatten() {
        let tag = item["tag"].as_str().unwrap_or("");
        if tag.is_empty() {
            continue;
        }
        if flag(&item["male"]) {
            prefixed.push(format!("male:{tag}"));
        } else if flag(&item["female"]) {
            prefixed.push(format!("female:{tag}"));
        } else {
            plain.push(tag.to_string());
        }
    }
    plain.sort();
    prefixed.sort();
    plain.append(&mut prefixed);
    plain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_matches_app_encoding() {
        let data = serde_json::json!({
            "title": "T", "type": "manga", "language": "korean",
            "artists": [{"artist": "a"}], "groups": [], "parodys": [{"parody": "original"}],
            "tags": [{"tag": "z"}, {"tag": "x", "female": "1"}, {"tag": "a"}, {"tag": "m", "male": 1}],
        });
        let g = compact(7, &data);
        assert_eq!(g.artists, r#"["a"]"#);
        assert_eq!(g.groups, "[]");
        assert_eq!(g.characters, "[]");
        assert_eq!(g.series, r#"["original"]"#);
        assert_eq!(g.tags, r#"["a","z","female:x","male:m"]"#);
        assert_eq!(g.language.as_deref(), Some("korean"));
    }

    #[test]
    fn thumbnail_path() {
        assert_eq!(
            thumbnail_url("abc123").unwrap(),
            format!("{THUMB_CDN}/webpbigtn/3/12/abc123.webp")
        );
        assert!(thumbnail_url("ab").is_none());
    }
}
