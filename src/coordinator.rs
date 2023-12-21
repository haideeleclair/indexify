use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use anyhow::Result;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tracing::{error, info};

use crate::{
    attribute_index::AttributeIndexManager,
    extractor::ExtractedEmbeddings,
    internal_api::{
        self,
        CreateWork,
        ExecutorMetadata,
        ExtractionEventPayload,
        ExtractorBinding,
        Task,
    },
    persistence::{ExtractedAttributes, Repository, Work},
    state::{store::Request, SharedState},
    vector_index::VectorIndexManager, entity::work,
};

pub struct Coordinator {
    // Executor ID -> Last Seen Timestamp
    executor_health_checks: Arc<RwLock<HashMap<String, u64>>>,

    executors: Arc<RwLock<HashMap<String, ExecutorMetadata>>>,

    // Extractor Name -> [Executor ID]
    extractors_table: Arc<RwLock<HashMap<String, Vec<String>>>>,

    repository: Arc<Repository>,

    vector_index_manager: Arc<VectorIndexManager>,

    attribute_index_manager: Arc<AttributeIndexManager>,

    pub shared_state: SharedState,

    tx: Sender<CreateWork>,
}

impl Coordinator {
    pub fn new(
        repository: Arc<Repository>,
        vector_index_manager: Arc<VectorIndexManager>,
        attribute_index_manager: Arc<AttributeIndexManager>,
        shared_state: SharedState,
    ) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(32);

        let coordinator = Arc::new(Self {
            executor_health_checks: Arc::new(RwLock::new(HashMap::new())),
            executors: Arc::new(RwLock::new(HashMap::new())),
            extractors_table: Arc::new(RwLock::new(HashMap::new())),
            repository,
            vector_index_manager,
            attribute_index_manager,
            shared_state,
            tx,
        });
        let coordinator_clone = coordinator.clone();
        tokio::spawn(async move {
            coordinator_clone.loop_for_work(rx).await.unwrap();
        });
        coordinator
    }

    pub async fn get_executors(&self) -> Result<Vec<ExecutorMetadata>> {
        let executors = self.executors.read().unwrap();
        Ok(executors.values().cloned().collect())
    }

    #[tracing::instrument(skip(self))]
    pub async fn process_extraction_events(&self) -> Result<(), anyhow::Error> {
        let events = self.shared_state.unprocessed_extraction_events().await?;
        for event in &events {
            info!("processing extraction event: {}", event.id.as_str());
            match &event.payload {
                ExtractionEventPayload::ExtractorBindingAdded { repository, id } => {
                    self.create_tasks_for_extractor_bindings(&repository, &id)
                        .await?;
                }
                ExtractionEventPayload::CreateContent { content_id } => {
                    if let Err(err) = self
                        .create_tasks_for_content(&event.repository_id, &content_id)
                        .await
                    {
                        error!("unable to create work: {}", &err.to_string());
                        return Err(err);
                    }
                }
            };

            self.shared_state
                .mark_extraction_event_processed(&event.id)
                .await?;
        }
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn create_tasks_for_extractor_bindings(
        &self,
        repository: &str,
        extractor_binding: &str,
    ) -> Result<(), anyhow::Error> {
        // Get all the content of this repository that matches predicates

        // Create tasks for each content
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn distribute_work(&self) -> Result<(), anyhow::Error> {
        let unassigned_tasks = self.shared_state.unassigned_tasks().await?;

        // work_id -> executor_id
        let mut task_assignments = HashMap::new();
        for task in unassigned_tasks {
            let executors = self
                .shared_state
                .get_executors_for_extractor(&task.extractor)
                .await?;
            let rand_index = rand::random::<usize>() % executors.len();
            if !executors.is_empty() {
                let executor_id = executors[rand_index].clone();
                task_assignments.insert(task.id.clone(), executor_id);
            }
        }
        info!("finishing work assignment: {:}", task_assignments.len());
        self.shared_state
            .commit_task_assignments(task_assignments)
            .await?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn create_tasks_for_content(
        &self,
        repository_id: &str,
        content_id: &str,
    ) -> Result<(), anyhow::Error> {
        let extractor_bindings = self
            .shared_state
            .filter_extractor_binding_for_content(content_id)
            .await?;
        let content_metadata = self.shared_state.get_conent_metadata(content_id).await?;
        let mut tasks = Vec::new();
        for extractor_binding in &extractor_bindings {
            let task = Task::new(
                &extractor_binding.extractor,
                repository_id,
                &content_metadata,
                extractor_binding.input_params.to_owned(),
            );
            tasks.push(task);
        }
        self.shared_state.create_tasks(tasks).await?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    #[cfg(test)]
    pub async fn record_extractor(
        &self,
        extractor: internal_api::ExtractorDescription,
    ) -> Result<(), anyhow::Error> {
        self.repository
            .record_extractors(vec![extractor.try_into().unwrap()])
            .await?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_work_for_worker(
        &self,
        executor_id: &str,
    ) -> Result<Vec<internal_api::Task>, anyhow::Error> {
        let tasks = self.shared_state.tasks_for_executor(executor_id).await?;
        Ok(tasks)
    }

    #[tracing::instrument(skip(self, rx))]
    async fn loop_for_work(&self, mut rx: Receiver<CreateWork>) -> Result<(), anyhow::Error> {
        info!("starting work distribution loop");
        loop {
            if (rx.recv().await).is_none() {
                info!("no work to process");
                return Ok(());
            }
            if let Err(err) = self.process_and_distribute_work().await {
                error!("unable to process and distribute work: {}", err.to_string());
            }
        }
    }

    #[tracing::instrument(skip(self))]
    pub async fn process_and_distribute_work(&self) -> Result<(), anyhow::Error> {
        info!("received work request, processing extraction events");
        self.process_extraction_events().await?;

        info!("doing distribution of work");
        self.distribute_work().await?;
        Ok(())
    }

    pub async fn get_executor(
        &self,
        extractor_name: &str,
    ) -> Result<ExecutorMetadata, anyhow::Error> {
        let extractors_table = self.extractors_table.read().unwrap();
        let executors = extractors_table.get(extractor_name).ok_or(anyhow::anyhow!(
            "no executors for extractor: {}",
            extractor_name
        ))?;
        let rand_index = rand::random::<usize>() % executors.len();
        let executor_id = executors[rand_index].clone();
        let executors = self.executors.read().unwrap();
        let executor = executors
            .get(&executor_id)
            .ok_or(anyhow::anyhow!("no executor found for id: {}", executor_id))?;
        Ok(executor.clone())
    }

    pub async fn publish_work(&self, work: CreateWork) -> Result<(), anyhow::Error> {
        self.tx.send(work).await?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn write_extracted_data(
        &self,
        work_status_list: Vec<internal_api::TaskStatus>,
    ) -> Result<()> {
        for work_status in work_status_list {
            let work = self
                .repository
                .update_work_state(&work_status.work_id, &work_status.status.into())
                .await?;
            for extracted_content in work_status.extracted_content {
                if let Some(feature) = extracted_content.feature.clone() {
                    let index_name = format!("{}-{}", work.extractor_binding, feature.name);
                    if let Some(text) = extracted_content.source_as_text() {
                        if let Some(embedding) = feature.embedding() {
                            let embeddings = ExtractedEmbeddings {
                                content_id: work.content_id.clone(),
                                text: text.clone(),
                                embeddings: embedding.clone(),
                            };
                            self.vector_index_manager
                                .add_embedding(&work.repository_id, &index_name, vec![embeddings])
                                .await?;
                        }
                    }
                    if let Some(metadata) = feature.metadata() {
                        let extracted_attributes = ExtractedAttributes::new(
                            &work.content_id,
                            metadata.clone(),
                            &work.extractor,
                        );
                        self.attribute_index_manager
                            .add_index(&work.repository_id, &index_name, extracted_attributes)
                            .await?;
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use crate::{
        blob_storage::BlobStorageBuilder,
        data_repository_manager::DataRepositoryManager,
        persistence::{ContentPayload, DataRepository, ExtractorBinding},
        test_util::{
            self,
            db_utils::{DEFAULT_TEST_EXTRACTOR, DEFAULT_TEST_REPOSITORY},
        },
    };

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_create_work() -> Result<(), anyhow::Error> {
        let db = test_util::db_utils::create_db().await.unwrap();
        let (vector_index_manager, extractor_executor, coordinator) =
            test_util::db_utils::create_index_manager(db.clone()).await;
        let blob_storage =
            BlobStorageBuilder::new_disk_storage("/tmp/indexify_test".to_string()).unwrap();
        let repository_manager =
            DataRepositoryManager::new_with_db(db.clone(), vector_index_manager, blob_storage);

        // Create a repository
        repository_manager
            .create(&DataRepository {
                name: DEFAULT_TEST_REPOSITORY.into(),
                data_connectors: vec![],
                metadata: HashMap::new(),
                extractor_bindings: vec![ExtractorBinding::new(
                    "test_extractor_binding",
                    DEFAULT_TEST_REPOSITORY,
                    DEFAULT_TEST_EXTRACTOR.into(),
                    vec![],
                    serde_json::json!({}),
                )],
            })
            .await?;

        repository_manager
            .add_texts(
                DEFAULT_TEST_REPOSITORY,
                vec![
                    ContentPayload::from_text(
                        DEFAULT_TEST_REPOSITORY,
                        "hello",
                        HashMap::from([("topic".to_string(), json!("pipe"))]),
                    ),
                    ContentPayload::from_text(
                        DEFAULT_TEST_REPOSITORY,
                        "world",
                        HashMap::from([("topic".to_string(), json!("baz"))]),
                    ),
                ],
            )
            .await?;

        // Insert a new worker and then create work
        coordinator.process_and_distribute_work().await.unwrap();

        let work_list = coordinator
            .get_work_for_worker(&extractor_executor.get_executor_info().id)
            .await?;

        // Check amount of work queued for the worker
        assert_eq!(work_list.len(), 2);
        Ok(())
    }
}
