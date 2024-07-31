use crate::*;
use async_walkdir::DirEntry;
use bytes::Bytes;
use futures::future::ready;
use futures::StreamExt;
use parking_lot::RwLock;
use std::collections::{hash_map, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
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
    sources: RwLock<HashSet<PathBuf>>,
    files: RwLock<Arc<FileData>>,
}

#[derive(uniffi::Record)]
pub struct UpdateStatus {
    pub found: u64,
    pub completed: u64,
    pub new: u64,
}

#[derive(Default)]
struct UpdateStatusAtomic {
    pub found: AtomicU64,
    pub completed: AtomicU64,
    pub new: AtomicU64,
}

impl UpdateStatusAtomic {
    fn resolve(&self) -> UpdateStatus {
        UpdateStatus {
            found: self.found.load(Ordering::SeqCst),
            completed: self.completed.load(Ordering::SeqCst),
            new: self.new.load(Ordering::SeqCst),
        }
    }
}

#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait ImportTreeCallback: Send + Sync + 'static {
    async fn callback(&self, status: UpdateStatus) -> Result<(), CallbackError>;
}

#[derive(uniffi::Object)]
pub struct Backend {
    node: Arc<Iroh>,
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
                Iroh::persistent(app_storage_path.join("iroh").display().to_string()).await?,
            ),
            doc_data: RwLock::new(load_doc_data().unwrap_or_default()),
            app_storage_path,
        })
    }

    #[uniffi::constructor(async_runtime = "tokio")]
    pub async fn create(iroh_database_path: String) -> Result<Self, IrohError> {
        Self::new(iroh_database_path).await
    }

    pub fn node(&self) -> Arc<Iroh> {
        self.node.clone()
    }

    pub fn num_files_for_document(&self, namespace: String) -> u64 {
        self.doc_data(namespace).files.read().len() as _
    }

    pub fn sources_for_document(&self, namespace: String) -> Vec<String> {
        self.doc_data(namespace)
            .sources
            .read()
            .iter()
            .map(|path| path.display().to_string())
            .collect()
    }

    pub fn add_source_to_document(&self, namespace: String, source: String) {
        self.doc_data(namespace)
            .sources
            .write()
            .insert(PathBuf::from(source));
    }

    pub fn remove_source_from_document(&self, namespace: String, source: String) {
        self.doc_data(namespace)
            .sources
            .write()
            .remove(&PathBuf::from(source));
    }

    #[uniffi::method(async_runtime = "tokio")]
    pub async fn add_file_tree(
        &self,
        doc: Arc<Doc>,
        namespace: String,
        author: Arc<AuthorId>,
        in_place: bool,
        recheck: bool,
        cb: Option<Arc<dyn ImportTreeCallback>>,
    ) -> Result<(), IrohError> {
        let status = &UpdateStatusAtomic::default();
        let cb = cb.as_ref();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();

        let (res_a, res_b) = tokio::join!(
            async move {
                while let Some(path) = rx.recv().await {
                    let path_str = (*path.to_string_lossy()).as_bytes().to_vec();
                    let key = Bytes::from(path_str);

                    let mut stream = doc
                        .inner
                        .import_file(author.0, key, path, in_place)
                        .await
                        .map_err(|err| anyhow::anyhow!("{}", err))?;

                    while stream.next().await.is_some() {}
                    status.completed.fetch_add(1, Ordering::SeqCst);
                    if let Some(cb) = cb.as_ref() {
                        cb.callback(status.resolve()).await?;
                    }
                }

                anyhow::Ok(())
            },
            async move {
                let doc_data = self.doc_data(namespace);

                let mut current = FileData::new();
                let old = doc_data.files.read().clone();
                let sources = doc_data.sources.read().clone();

                for source in sources.iter() {
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
                        if !entry
                            .file_type()
                            .await
                            .map_err(|err| anyhow::anyhow!("{}", err))?
                            .is_file()
                        {
                            continue;
                        }

                        let path = entry.path();
                        if let hash_map::Entry::Vacant(map_entry) = current.entry(path.clone()) {
                            let metadata = entry
                                .metadata()
                                .await
                                .map_err(|err| anyhow::anyhow!("{}", err))?;
                            let modified = metadata
                                .modified()
                                .map_err(|err| anyhow::anyhow!("{}", err))?;
                            let data = (metadata.len(), modified);
                            map_entry.insert(data);

                            let updated = match old.get(&path) {
                                Some(old_data) => old_data != &data,
                                None => true,
                            };

                            status.found.fetch_add(1, Ordering::SeqCst);

                            if updated || recheck {
                                tx.send(path).unwrap();
                                status.new.fetch_add(1, Ordering::SeqCst);
                            }

                            if let Some(cb) = cb.as_ref() {
                                cb.callback(status.resolve()).await?;
                            }
                        }
                    }
                }

                *doc_data.files.write() = Arc::new(current);

                self.write_sources()?;

                anyhow::Ok(())
            }
        );

        res_a?;
        res_b?;
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
    let qr = qrcode::QrCode::new(string).map_err(|err| anyhow::anyhow!("{}", err))?;

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
