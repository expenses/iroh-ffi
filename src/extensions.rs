use crate::*;
use async_walkdir::DirEntry;
use bytes::Bytes;
use futures::future::ready;
use futures::StreamExt;
use parking_lot::RwLock;
use std::collections::{hash_map, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

fn is_hidden(entry: &DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|s| s.starts_with('.'))
        .unwrap_or(false)
}

type FileData = HashMap<PathBuf, (u64, SystemTime)>;

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct DocData {
    sources: RwLock<HashSet<String>>,
    files: RwLock<Arc<FileData>>,
}

impl DocData {
    async fn update(&self, recheck: bool) -> Result<Vec<PathBuf>, anyhow::Error> {
        let mut current = FileData::new();
        let old = self.files.read().clone();
        let mut to_update = Vec::new();

        let sources = self.sources.read().clone();

        for source in sources {
            let mut stream = async_walkdir::WalkDir::new(source)
                .filter(|e| {
                    ready(if is_hidden(&e) {
                        async_walkdir::Filtering::IgnoreDir
                    } else {
                        async_walkdir::Filtering::Continue
                    })
                })
                .filter_map(|e| ready(e.ok()));

            while let Some(entry) = stream.next().await {
                if !entry.file_type().await?.is_file() {
                    continue;
                }

                let path = PathBuf::from(entry.path());
                if let hash_map::Entry::Vacant(map_entry) = current.entry(path.clone()) {
                    let metadata = entry.metadata().await?;
                    let modified = metadata.modified()?;
                    let data = (metadata.len(), modified);
                    map_entry.insert(data);

                    let updated = match old.get(&path) {
                        Some(old_data) => old_data != &data,
                        None => true,
                    };

                    if updated || recheck {
                        to_update.push(path);
                    }
                }
            }
        }

        *self.files.write() = Arc::new(current);

        Ok(to_update)
    }
}

#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait ImportTreeCallback: Send + Sync + 'static {
    async fn progress(&self) -> Result<(), CallbackError>;
    async fn to_update(&self, to_update: u64) -> Result<(), CallbackError>;
    async fn total(&self, total: u64) -> Result<(), CallbackError>;
}

#[derive(uniffi::Object)]
pub struct Backend {
    node: Arc<IrohNode>,
    doc_data: RwLock<HashMap<String, Arc<DocData>>>,
    app_storage_path: PathBuf,
}

impl Backend {
    fn doc_data(&self, namespace: String) -> Arc<DocData> {
        self.doc_data.write().entry(namespace).or_default().clone()
    }

    fn write_sources(&self) -> anyhow::Result<()> {
        serde_json::to_writer(
            std::fs::File::create(self.app_storage_path.join("sources.json"))?,
            &self.doc_data,
        )?;
        Ok(())
    }
}

#[uniffi::export]
impl Backend {
    #[uniffi::constructor(async_runtime = "tokio")]
    pub async fn new(app_storage_path: String) -> Result<Self, IrohError> {
        let app_storage_path = PathBuf::from(app_storage_path);

        let load_doc_data = || -> anyhow::Result<HashMap<String, Arc<DocData>>> {
            let doc_data = serde_json::from_reader(std::fs::File::open(
                app_storage_path.join("sources.json"),
            )?)?;
            Ok(doc_data)
        };

        Ok(Self {
            node: Arc::new(
                IrohNode::persistent(app_storage_path.join("iroh").display().to_string()).await?,
            ),
            doc_data: RwLock::new(load_doc_data().unwrap_or_default()),
            app_storage_path,
        })
    }

    #[uniffi::constructor(async_runtime = "tokio")]
    pub async fn create(iroh_database_path: String) -> Result<Self, IrohError> {
        Self::new(iroh_database_path).await
    }

    pub fn node(&self) -> Arc<IrohNode> {
        self.node.clone()
    }

    pub fn sources_for_document(&self, namespace: String) -> Vec<String> {
        self.doc_data(namespace)
            .sources
            .read()
            .iter()
            .cloned()
            .collect()
    }

    pub fn add_source_to_document(&self, namespace: String, source: String) {
        self.doc_data(namespace).sources.write().insert(source);
    }

    pub fn remove_source_from_document(&self, namespace: String, source: String) {
        self.doc_data(namespace).sources.write().remove(&source);
    }

    pub fn serialize(&self) -> Result<String, IrohError> {
        serde_json::to_string(&self.doc_data)
            .map_err(|err| IrohError::from(anyhow::anyhow!("{}", err)))
    }

    #[uniffi::method(async_runtime = "tokio")]
    pub async fn add_file_tree(
        &self,
        doc: &Doc,
        namespace: String,
        author: Arc<AuthorId>,
        in_place: bool,
        recheck: bool,
        cb: Option<Arc<dyn ImportTreeCallback>>,
    ) -> Result<(), IrohError> {
        let doc_data = self.doc_data(namespace);
        let to_update = doc_data.update(recheck).await?;
        self.write_sources()?;

        if let Some(ref cb) = cb {
            cb.to_update(to_update.len() as _).await?;
            let total = doc_data.files.read().len() as _;
            cb.total(total).await?;
        }

        for path in to_update {
            let path_str = (*path.to_string_lossy()).as_bytes().to_vec();
            let key = Bytes::from(path_str);

            let mut stream = doc.inner.import_file(author.0, key, path, in_place).await?;

            while stream.next().await.is_some() {}

            if let Some(ref cb) = cb {
                cb.progress().await?;
            }
        }

        Ok(())
    }
}

#[derive(uniffi::Record)]
pub struct QrCode {
    bytes: Vec<u8>,
    width: i32,
}

#[uniffi::export]
pub fn create_qr_code(string: String) -> Result<QrCode, IrohError> {
    let qr = qrcode::QrCode::new(&string).map_err(|err| anyhow::anyhow!("{}", err))?;

    let image = qr
        .render::<image::Luma<u8>>()
        .dark_color(image::Luma([255]))
        .light_color(image::Luma([0]))
        .quiet_zone(false)
        .build();

    Ok(QrCode {
        width: image.width() as _,
        bytes: image.into_raw(),
    })
}
