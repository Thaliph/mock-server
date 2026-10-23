//! Forme de mock des snapshots servie aux sidecars (spec §5.6) : calculée une fois par
//! snapshot, compressée d'avance en `zstd` et `gzip`, gardée en mémoire sous une borne en
//! octets. Des demandes simultanées du même snapshot attendent le même calcul.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use mimic_core::snapshot::{MockSnapshot, Snapshot, digest};
use tokio::sync::OnceCell;

/// Mémoire tenue par les formes de mock calculées, versions compressées comprises.
pub const MAX_CACHE_BYTES: usize = 128 * 1024 * 1024;
/// Niveaux de compression : le calcul n'est fait qu'une fois par snapshot.
const ZSTD_LEVEL: i32 = 9;
const GZIP_LEVEL: u32 = 9;

/// Forme de mock compressée. Seules les versions compressées sont gardées : la forme telle
/// quelle, rarement demandée (un sidecar envoie toujours `Accept-Encoding`), est
/// décompressée à la demande.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedMock {
    /// Empreinte de la forme sérialisée (`ETag`, sans guillemets).
    pub etag: String,
    /// Taille de la forme sérialisée, sans compression.
    pub identity_len: usize,
    pub zstd: Bytes,
    pub gzip: Bytes,
}

impl EncodedMock {
    /// Déduit la forme de mock du snapshot complet `full` (JSONL), après avoir vérifié son
    /// empreinte, puis la compresse.
    pub fn from_full(full: &[u8], id: &str) -> Result<Self, String> {
        let snapshot = Snapshot::from_jsonl(full, id)?;
        let identity = MockSnapshot::from_snapshot(&snapshot).to_json();
        let zstd = zstd::bulk::compress(&identity, ZSTD_LEVEL).map_err(|e| format!("zstd: {e}"))?;
        let mut gz =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(GZIP_LEVEL));
        gz.write_all(&identity).map_err(|e| format!("gzip: {e}"))?;
        let gzip = gz.finish().map_err(|e| format!("gzip: {e}"))?;
        Ok(Self {
            etag: digest(&identity),
            identity_len: identity.len(),
            zstd: zstd.into(),
            gzip: gzip.into(),
        })
    }

    /// Octets tenus en mémoire.
    pub fn size(&self) -> usize {
        self.zstd.len() + self.gzip.len()
    }

    /// Forme sérialisée telle quelle, décompressée : calcul à faire hors des threads du
    /// serveur.
    pub fn identity(&self) -> Result<Vec<u8>, String> {
        zstd::decode_all(&self.zstd[..]).map_err(|e| format!("zstd: {e}"))
    }
}

/// Encodage de la réponse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Zstd,
    Gzip,
    Identity,
}

impl Encoding {
    /// Valeur de `Content-Encoding`, absente sans compression.
    pub fn header(self) -> Option<&'static str> {
        match self {
            Encoding::Zstd => Some("zstd"),
            Encoding::Gzip => Some("gzip"),
            Encoding::Identity => None,
        }
    }
}

/// Choisit l'encodage d'après `Accept-Encoding` : celui de plus grand `q`, `zstd` puis `gzip`
/// à égalité ; sans compression si aucun des deux n'est accepté (`q=0` refuse).
pub fn pick(accept: Option<&str>) -> Encoding {
    let mut zstd = None;
    let mut gzip = None;
    let mut any = None;
    for item in accept.unwrap_or_default().split(',') {
        let mut parts = item.split(';');
        let name = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
        let q = parts
            .filter_map(|p| p.trim().strip_prefix("q="))
            .find_map(|v| v.trim().parse::<f32>().ok())
            .unwrap_or(1.0);
        match name.as_str() {
            "zstd" => zstd = Some(q),
            "gzip" | "x-gzip" => gzip = Some(q),
            "*" => any = Some(q),
            _ => {}
        }
    }
    let (z, g) = (zstd.or(any).unwrap_or(0.0), gzip.or(any).unwrap_or(0.0));
    if z > 0.0 && z >= g {
        Encoding::Zstd
    } else if g > 0.0 {
        Encoding::Gzip
    } else {
        Encoding::Identity
    }
}

/// Vrai si `If-None-Match` désigne déjà cet `ETag` (entre guillemets).
pub fn not_modified(if_none_match: Option<&str>, etag: &str) -> bool {
    if_none_match.is_some_and(|v| {
        v.split(',')
            .map(|t| t.trim().trim_start_matches("W/"))
            .any(|t| t == "*" || t == etag)
    })
}

type Cell = Arc<OnceCell<Option<Arc<EncodedMock>>>>;

/// Formes calculées, les plus anciennes évincées au-delà de la borne en octets.
pub struct MockForms {
    inner: Mutex<Inner>,
    max_bytes: usize,
}

#[derive(Default)]
struct Inner {
    cells: BTreeMap<String, Cell>,
    /// Formes calculées, de la plus ancienne à la plus récente, avec leur taille.
    order: VecDeque<(String, usize)>,
    bytes: usize,
}

impl MockForms {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            max_bytes,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Forme gardée sous `key`, sinon calculée par `compute` ; un échec n'est pas gardé.
    pub async fn get<F, Fut>(&self, key: &str, compute: F) -> Option<Arc<EncodedMock>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Option<EncodedMock>>,
    {
        let cell = self
            .lock()
            .cells
            .entry(key.to_string())
            .or_default()
            .clone();
        let got = cell
            .get_or_init(|| async { compute().await.map(Arc::new) })
            .await
            .clone();
        let mut inner = self.lock();
        let current = inner.cells.get(key).is_some_and(|c| Arc::ptr_eq(c, &cell));
        if !current {
            return got;
        }
        match &got {
            None => {
                inner.cells.remove(key);
            }
            Some(form) if !inner.order.iter().any(|(k, _)| k == key) => {
                let size = form.size();
                inner.order.push_back((key.to_string(), size));
                inner.bytes += size;
                // La plus récente reste, même seule au-delà de la borne.
                while inner.bytes > self.max_bytes && inner.order.len() > 1 {
                    let Some((old, size)) = inner.order.pop_front() else {
                        break;
                    };
                    inner.cells.remove(&old);
                    inner.bytes -= size;
                }
            }
            Some(_) => {}
        }
        got
    }

    /// Nombre de formes gardées et octets tenus.
    pub fn usage(&self) -> (usize, usize) {
        let inner = self.lock();
        (inner.order.len(), inner.bytes)
    }

    /// Retire la forme gardée sous `key`, si elle existe (CTL-44, snapshot purgé : sa forme
    /// en cache ne doit plus être servie une fois le contenu supprimé).
    pub fn evict(&self, key: &str) {
        let mut inner = self.lock();
        inner.cells.remove(key);
        if let Some(pos) = inner.order.iter().position(|(k, _)| k == key) {
            let (_, size) = inner.order.remove(pos).expect("position valide");
            inner.bytes -= size;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctl_23_accept_encoding_prefers_zstd_then_gzip() {
        assert_eq!(pick(None), Encoding::Identity);
        assert_eq!(pick(Some("")), Encoding::Identity);
        assert_eq!(pick(Some("br, deflate")), Encoding::Identity);
        assert_eq!(pick(Some("gzip")), Encoding::Gzip);
        assert_eq!(pick(Some("gzip, zstd")), Encoding::Zstd);
        assert_eq!(pick(Some("zstd;q=0.5, gzip")), Encoding::Gzip);
        assert_eq!(pick(Some("gzip;q=1, zstd;q=0")), Encoding::Gzip);
        assert_eq!(pick(Some("gzip;q=0, zstd;q=0")), Encoding::Identity);
        assert_eq!(pick(Some("*")), Encoding::Zstd);
        assert_eq!(pick(Some("*;q=0.1, gzip")), Encoding::Gzip);
    }

    #[test]
    fn ctl_23_if_none_match() {
        assert!(not_modified(Some("\"ab\""), "\"ab\""));
        assert!(not_modified(Some("W/\"ab\", \"cd\""), "\"cd\""));
        assert!(not_modified(Some("*"), "\"cd\""));
        assert!(!not_modified(Some("\"ab\""), "\"cd\""));
        assert!(!not_modified(None, "\"cd\""));
    }

    fn form(n: usize) -> EncodedMock {
        EncodedMock {
            etag: n.to_string(),
            identity_len: n,
            zstd: vec![0; n].into(),
            gzip: Bytes::new(),
        }
    }

    #[test]
    fn identity_is_the_decompressed_form() {
        let full = mimic_core::snapshot::build(
            Vec::<mimic_core::model::Exchange>::new(),
            &Default::default(),
        );
        let form = EncodedMock::from_full(&full.to_jsonl(), &full.id).unwrap();
        let expected = MockSnapshot::from_snapshot(&full).to_json();
        assert_eq!(form.identity().unwrap(), expected);
        assert_eq!(form.etag, digest(&expected));
        assert!(EncodedMock::from_full(b"", "0000000000000000").is_err());
    }

    #[tokio::test]
    async fn computed_once_failures_not_kept_oldest_evicted() {
        let forms = MockForms::new(100);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let compute = |n: usize| {
            let calls = &calls;
            move || async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some(form(n))
            }
        };
        let (a, b) = tokio::join!(forms.get("a", compute(60)), forms.get("a", compute(60)));
        assert_eq!(a, b);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(forms.usage(), (1, 60));

        assert_eq!(forms.get("x", || async { None }).await, None);
        assert_eq!(forms.get("x", compute(10)).await.unwrap().etag, "10");

        // « b » fait dépasser la borne : « a », la plus ancienne, est évincée.
        forms.get("b", compute(50)).await.unwrap();
        assert_eq!(forms.usage(), (2, 60));
        forms.get("a", compute(60)).await.unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 4);
    }
}
