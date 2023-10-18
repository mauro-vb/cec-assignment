use async_broadcast::broadcast;
use clap::ArgMatches;
use futures::future;
use rand::Rng;
use serde::Deserialize;
use std::{fs, sync::Arc};
use tokio::{
    sync::{mpsc::Receiver, RwLock},
    time::{self, Duration},
};

use crate::experiment::ExperimentDocument;
use crate::generator::{self, APIQuery};
use crate::metric::{MetricServer, Metrics};
use crate::requests::{Host, Requestor};

#[derive(Deserialize)]
pub struct ExperimentReceiverConfig {
    hosts: Vec<Host>,
}

impl ExperimentReceiverConfig {
    pub fn from_file(file: &str) -> Self {
        let content =
            fs::read_to_string(file).expect(format!("File `{}` should exist", file).as_str());
        let hosts: Vec<Host> =
            serde_json::from_str(content.as_str()).expect("Could not deserialize config file");
        Self { hosts }
    }
}

impl From<&mut ArgMatches> for ExperimentReceiverConfig {
    fn from(args: &mut ArgMatches) -> Self {
        let file = args.remove_one::<String>("hosts-file").expect("Required");
        Self::from_file(file.as_str())
    }
}

pub struct ExperimentReceiver {
    config: ExperimentReceiverConfig,
    experiment_rx: Receiver<ExperimentDocument>,
    experiments: Arc<RwLock<Vec<Arc<RwLock<ExperimentDocument>>>>>,
}

impl ExperimentReceiver {
    pub fn new(
        config: ExperimentReceiverConfig,
        experiment_rx: Receiver<ExperimentDocument>,
    ) -> Self {
        Self {
            config,
            experiment_rx,
            experiments: Arc::new(RwLock::new(vec![])),
        }
    }

    async fn add_first_experiment(&mut self) {
        let experiment = self.experiment_rx.recv().await.expect("Sender available");
        self.experiments
            .write()
            .await
            .push(Arc::new(RwLock::new(experiment)));
    }

    /// Spawn 1 thread per group
    /// TODO: Parametrized start wait
    fn create_requestors(
        &self,
        batch_rx: async_broadcast::Receiver<Arc<Vec<APIQuery>>>,
        metrics: Metrics,
    ) {
        for host in self.config.hosts.iter() {
            let batch_rx = batch_rx.clone();
            let mut requestor = Requestor::new(host.clone(), batch_rx, metrics.clone());

            tokio::spawn(async move {
                time::sleep(Duration::from_millis(5000)).await;
                requestor.start().await;
            });
        }
    }

    pub async fn start(mut self) {
        let (batch_tx, batch_rx) = broadcast::<Arc<Vec<APIQuery>>>(1000);

        // Create a sample counter metric family utilizing the above custom label
        // type, representing the number of HTTP requests received.
        let metrics = Metrics::new();
        let metric_server = MetricServer::new(metrics.clone());
        metric_server.start();

        self.add_first_experiment().await;

        self.create_requestors(batch_rx, metrics);

        // Each iteration receives messages and generates load for the next minute
        // Sleep for 60 seconds after generating all the requests, if there are new experiments, read
        // them and generate the load, otherwise just generate the load with the experiments available.
        //
        // TODO: Parametrizable MAX_BATCH_SIZE
        loop {
            self.receive_experiments().await;
            let mut handles = vec![];
            handles.push(tokio::spawn(async move {
                time::sleep(Duration::from_millis(60 * 1000)).await;
            }));
            let batch_size = {
                let mut rng = rand::thread_rng();
                rng.gen_range(100..200)
            };
            handles.append(
                &mut (0..60)
                    .map(|_| {
                        let experiments = self.experiments.clone();
                        let batch_tx = batch_tx.clone();
                        tokio::spawn(async move {
                            generator::generate(experiments.clone(), batch_size, batch_tx).await
                        })
                    })
                    .collect(),
            );
            future::join_all(handles).await;
        }
    }

    async fn receive_experiments(&mut self) {
        let mut experiments = self.experiments.write().await;
        while let Ok(experiment) = self.experiment_rx.try_recv() {
            experiments.push(Arc::new(RwLock::new(experiment)));
        }
    }
}
