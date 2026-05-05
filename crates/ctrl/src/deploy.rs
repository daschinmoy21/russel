use std::{
    io::{Read, Write},
    net::{Shutdown, TcpStream},
    path::PathBuf,
    time::{Duration, Instant},
};

use russel_core::{
    api::{DeployRequest, DeployResponse, PortMapping},
    config::Russelfile,
};

use crate::{
    build::NixBuilder,
    database::DatabaseProvisioner,
    git::GitClient,
    health::HealthChecker,
    microvm::{MicrovmConfigGenerator, MicrovmRunner},
    network::PortAllocator,
    state::AppState,
    traefik::TraefikClient,
};

#[derive(Debug)]
pub struct DeployPipeline {
    state: AppState,
    git: GitClient,
    builder: NixBuilder,
    database: DatabaseProvisioner,
    microvm: MicrovmConfigGenerator,
    runner: MicrovmRunner,
    ports: PortAllocator,
    traefik: TraefikClient,
    health: HealthChecker,
}

impl DeployPipeline {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            git: GitClient::default(),
            builder: NixBuilder,
            database: DatabaseProvisioner,
            microvm: MicrovmConfigGenerator::default(),
            runner: MicrovmRunner,
            ports: PortAllocator::default(),
            traefik: TraefikClient::default(),
            health: HealthChecker,
        }
    }

    pub async fn deploy(&self, request: DeployRequest) -> DeployResponse {
        let started = Instant::now();
        let service_id = request.vm_id.clone().unwrap_or_else(|| "api".to_string());
        let vm_id = service_id.clone();
        self.state.mark_building(&service_id);

        let result = self.deploy_inner(&service_id, request).await;

        match result {
            Ok(output) => {
                self.state.mark_deployed(&service_id, output.child);
                DeployResponse {
                    service_id,
                    vm_id,
                    status: "deployed".to_string(),
                    store_path: Some(output.store_path.display().to_string()),
                    microvm_config_path: Some(output.microvm_config_path.display().to_string()),
                    runner_path: Some(output.runner_path.display().to_string()),
                    port: Some(output.port),
                    elapsed_ms: started.elapsed().as_millis(),
                    message:
                        "microVM is running through microvm.nix; forwarded port passed health check"
                            .to_string(),
                }
            }
            Err(error) => {
                self.state.mark_failed(&service_id, error.to_string());
                DeployResponse {
                    service_id,
                    vm_id,
                    status: "failed".to_string(),
                    store_path: None,
                    microvm_config_path: None,
                    runner_path: None,
                    port: None,
                    elapsed_ms: started.elapsed().as_millis(),
                    message: error.to_string(),
                }
            }
        }
    }

    async fn deploy_inner(
        &self,
        service_id: &str,
        request: DeployRequest,
    ) -> anyhow::Result<DeployOutput> {
        let repo_path = self.git.clone_or_use_local(&request.repo_url).await?;
        let config_path = repo_path.join(PathBuf::from(request.config_path));
        let config = Russelfile::load(&config_path)?;

        if let Some(database) = &config.database {
            self.database.ensure(database).await?;
        }

        let build = self.builder.build(&repo_path).await?;
        let port = request.port.unwrap_or_else(|| PortMapping {
            host: self.ports.next(),
            guest: config.service.port,
        });
        let vm_config = self.microvm.generate(
            service_id,
            &config,
            &build.store_path,
            port.host,
            port.guest,
        )?;
        vm_config.persist()?;
        let runner_path = self.runner.build_runner(&vm_config).await?;
        let mut started = self.runner.start(&vm_config, runner_path).await?;

        self.traefik.register(service_id, port.host).await?;
        let _healthy = self
            .health
            .check(&format!("http://127.0.0.1:{}/health", port.host))
            .await;
        if !wait_for_health(&mut started.child, port.host, Duration::from_secs(60)) {
            let _ = started.child.kill().await;
            anyhow::bail!(
                "microVM started but health check did not pass on http://127.0.0.1:{}/health; see {}",
                port.host,
                vm_config.log_path.display()
            );
        }
        let microvm_config_path = vm_config.path.clone();
        self.state.attach_vm_config(service_id, vm_config);

        Ok(DeployOutput {
            store_path: build.store_path,
            microvm_config_path,
            runner_path: started.runner_path,
            child: started.child,
            port,
        })
    }
}

#[derive(Debug)]
struct DeployOutput {
    store_path: PathBuf,
    microvm_config_path: PathBuf,
    runner_path: PathBuf,
    child: tokio::process::Child,
    port: PortMapping,
}

fn wait_for_health(child: &mut tokio::process::Child, host_port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return false;
        }

        if health_request(host_port) {
            return true;
        }

        std::thread::sleep(Duration::from_millis(250));
    }

    false
}

fn health_request(host_port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(
        &format!("127.0.0.1:{host_port}")
            .parse()
            .expect("valid localhost socket address"),
        Duration::from_millis(250),
    ) else {
        return false;
    };

    let request = b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    if stream.write_all(request).is_err() {
        return false;
    }
    let _ = stream.shutdown(Shutdown::Write);

    let mut response = String::new();
    stream.read_to_string(&mut response).is_ok() && response.starts_with("HTTP/1.1 200")
}
