// Copyright (c) 2019 Ant Financial
//
// SPDX-License-Identifier: Apache-2.0
//

use async_trait::async_trait;
use rustjail::{pipestream::PipeStream, process::StreamType};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf};
use tokio::sync::Mutex;

use std::ffi::CString;
use std::io;
use std::path::Path;
use std::sync::Arc;
use ttrpc::{
    self,
    error::get_rpc_status,
    r#async::{Server as TtrpcServer, TtrpcContext},
};

use anyhow::{anyhow, Context, Result};
use cgroups::freezer::FreezerState;
use oci::{LinuxNamespace, Root, Spec};
use protobuf::{Message, RepeatedField, SingularPtrField};
use protocols::agent::{
    AddSwapRequest, AgentDetails, CopyFileRequest, GuestDetailsResponse, Interfaces, Metrics,
    OOMEvent, ReadStreamResponse, Routes, StatsContainerResponse, VolumeStatsRequest,
    WaitProcessResponse, WriteStreamResponse,
};
use protocols::csi::{VolumeCondition, VolumeStatsResponse, VolumeUsage, VolumeUsage_Unit};
use protocols::empty::Empty;
use protocols::health::{
    HealthCheckResponse, HealthCheckResponse_ServingStatus, VersionCheckResponse,
};
use protocols::types::Interface;
use rustjail::cgroups::notifier;
use rustjail::container::{BaseContainer, Container, LinuxContainer};
use rustjail::process::Process;
use rustjail::specconv::CreateOpts;

use nix::errno::Errno;
use nix::mount::MsFlags;
use nix::sys::stat;
use nix::unistd::{self, Pid};
use rustjail::cgroups::Manager;
use rustjail::process::ProcessOperations;

use sysinfo::{DiskExt, System, SystemExt};

use crate::device::{
    add_devices, get_virtio_blk_pci_device_name, update_device_cgroup, update_env_pci,
};
use crate::image_rpc;
use crate::linux_abi::*;
use crate::metrics::get_metrics;
use crate::mount::{add_storages, baremount, STORAGE_HANDLER_LIST};
use crate::namespace::{NSTYPEIPC, NSTYPEPID, NSTYPEUTS};
use crate::network::setup_guest_dns;
use crate::pci;
use crate::random;
use crate::sandbox::Sandbox;
use crate::version::{AGENT_VERSION, API_VERSION};
use crate::AGENT_CONFIG;

use crate::trace_rpc_call;
use crate::tracer::extract_carrier_from_ttrpc;
use opentelemetry::global;
use tracing::span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use tracing::instrument;

use libc::{self, c_char, c_ushort, pid_t, winsize, TIOCSWINSZ};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::prelude::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use nix::unistd::{Gid, Uid};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

pub const CONTAINER_BASE: &str = "/run/kata-containers";
const MODPROBE_PATH: &str = "/sbin/modprobe";
const ANNO_K8S_IMAGE_NAME: &str = "io.kubernetes.cri.image-name";
const CONFIG_JSON: &str = "config.json";

const ERR_INVALID_BLOCK_SIZE: &str = "Invalid block size";

// Convenience macro to obtain the scope logger
macro_rules! sl {
    () => {
        slog_scope::logger()
    };
}

// Convenience macro to wrap an error and response to ttrpc client
macro_rules! ttrpc_error {
    ($code:path, $err:expr $(,)?) => {
        get_rpc_status($code, format!("{:?}", $err))
    };
}

macro_rules! is_allowed {
    ($req:ident) => {
        if !AGENT_CONFIG
            .read()
            .await
            .is_allowed_endpoint($req.descriptor().name())
        {
            return Err(ttrpc_error!(
                ttrpc::Code::UNIMPLEMENTED,
                format!("{} is blocked", $req.descriptor().name()),
            ));
        }
    };
}

#[derive(Clone, Debug)]
pub struct AgentService {
    sandbox: Arc<Mutex<Sandbox>>,
}

// A container ID must match this regex:
//
//     ^[a-zA-Z0-9][a-zA-Z0-9_.-]+$
//
pub fn verify_cid(id: &str) -> Result<()> {
    let mut chars = id.chars();

    let valid = match chars.next() {
        Some(first)
            if first.is_alphanumeric()
                && id.len() > 1
                && chars.all(|c| c.is_alphanumeric() || ['.', '-', '_'].contains(&c)) =>
        {
            true
        }
        _ => false,
    };

    match valid {
        true => Ok(()),
        false => Err(anyhow!("invalid container ID: {:?}", id)),
    }
}

// Partially merge an OCI process specification into another one.
fn merge_oci_process(target: &mut oci::Process, source: &oci::Process) {
    if target.args.is_empty() && !source.args.is_empty() {
        target.args.append(&mut source.args.clone());
    }

    if target.cwd.is_empty() && !source.cwd.is_empty() {
        target.cwd = String::from(&source.cwd);
    }

    target.env.append(&mut source.env.clone());
}

impl AgentService {
    #[instrument]
    async fn do_create_container(
        &self,
        req: protocols::agent::CreateContainerRequest,
    ) -> Result<()> {
        let cid = req.container_id.clone();

        verify_cid(&cid)?;

        let mut oci_spec = req.OCI.clone();
        let use_sandbox_pidns = req.get_sandbox_pidns();

        let sandbox;
        let mut s;

        let mut oci = match oci_spec.as_mut() {
            Some(spec) => rustjail::grpc_to_oci(spec),
            None => {
                error!(sl!(), "no oci spec in the create container request!");
                return Err(anyhow!(nix::Error::EINVAL));
            }
        };

        info!(sl!(), "receive createcontainer, spec: {:?}", &oci);
        info!(
            sl!(),
            "receive createcontainer, storages: {:?}", &req.storages
        );

        // Merge the image bundle OCI spec into the container creation request OCI spec.
        self.merge_bundle_oci(&mut oci).await?;

        // Some devices need some extra processing (the ones invoked with
        // --device for instance), and that's what this call is doing. It
        // updates the devices listed in the OCI spec, so that they actually
        // match real devices inside the VM. This step is necessary since we
        // cannot predict everything from the caller.
        add_devices(&req.devices.to_vec(), &mut oci, &self.sandbox).await?;

        // Both rootfs and volumes (invoked with --volume for instance) will
        // be processed the same way. The idea is to always mount any provided
        // storage to the specified MountPoint, so that it will match what's
        // inside oci.Mounts.
        // After all those storages have been processed, no matter the order
        // here, the agent will rely on rustjail (using the oci.Mounts
        // list) to bind mount all of them inside the container.
        let m = add_storages(
            sl!(),
            req.storages.to_vec(),
            self.sandbox.clone(),
            Some(req.container_id.clone()),
        )
        .await?;
        {
            sandbox = self.sandbox.clone();
            s = sandbox.lock().await;
            s.container_mounts.insert(cid.clone(), m);
        }

        update_container_namespaces(&s, &mut oci, use_sandbox_pidns)?;

        // Add the root partition to the device cgroup to prevent access
        update_device_cgroup(&mut oci)?;

        // Append guest hooks
        append_guest_hooks(&s, &mut oci)?;

        // write spec to bundle path, hooks might
        // read ocispec
        let olddir = setup_bundle(&cid, &mut oci)?;
        // restore the cwd for kata-agent process.
        defer!(unistd::chdir(&olddir).unwrap());

        let opts = CreateOpts {
            cgroup_name: "".to_string(),
            use_systemd_cgroup: false,
            no_pivot_root: s.no_pivot_root,
            no_new_keyring: false,
            spec: Some(oci.clone()),
            rootless_euid: false,
            rootless_cgroup: false,
        };

        let mut ctr: LinuxContainer =
            LinuxContainer::new(cid.as_str(), CONTAINER_BASE, opts, &sl!())?;

        let pipe_size = AGENT_CONFIG.read().await.container_pipe_size;

        let p = if let Some(p) = oci.process {
            Process::new(&sl!(), &p, cid.as_str(), true, pipe_size)?
        } else {
            info!(sl!(), "no process configurations!");
            return Err(anyhow!(nix::Error::EINVAL));
        };
        ctr.start(p).await?;
        s.update_shared_pidns(&ctr)?;
        s.add_container(ctr);
        info!(sl!(), "created container!");

        Ok(())
    }

    #[instrument]
    async fn do_start_container(&self, req: protocols::agent::StartContainerRequest) -> Result<()> {
        let cid = req.container_id;

        let sandbox = self.sandbox.clone();
        let mut s = sandbox.lock().await;
        let sid = s.id.clone();

        let ctr = s
            .get_container(&cid)
            .ok_or_else(|| anyhow!("Invalid container id"))?;

        ctr.exec()?;

        if sid == cid {
            return Ok(());
        }

        // start oom event loop
        if let Some(ref ctr) = ctr.cgroup_manager {
            let cg_path = ctr.get_cg_path("memory");

            if let Some(cg_path) = cg_path {
                let rx = notifier::notify_oom(cid.as_str(), cg_path.to_string()).await?;

                s.run_oom_event_monitor(rx, cid.clone()).await;
            }
        }

        Ok(())
    }

    #[instrument]
    async fn do_remove_container(
        &self,
        req: protocols::agent::RemoveContainerRequest,
    ) -> Result<()> {
        let cid = req.container_id.clone();
        let mut cmounts: Vec<String> = vec![];

        let mut remove_container_resources = |sandbox: &mut Sandbox| -> Result<()> {
            // Find the sandbox storage used by this container
            let mounts = sandbox.container_mounts.get(&cid);
            if let Some(mounts) = mounts {
                for m in mounts.iter() {
                    if sandbox.storages.get(m).is_some() {
                        cmounts.push(m.to_string());
                    }
                }
            }

            for m in cmounts.iter() {
                sandbox.unset_and_remove_sandbox_storage(m)?;
            }

            sandbox.container_mounts.remove(cid.as_str());
            sandbox.containers.remove(cid.as_str());
            Ok(())
        };

        if req.timeout == 0 {
            let s = Arc::clone(&self.sandbox);
            let mut sandbox = s.lock().await;

            sandbox.bind_watcher.remove_container(&cid).await;

            sandbox
                .get_container(&cid)
                .ok_or_else(|| anyhow!("Invalid container id"))?
                .destroy()
                .await?;

            remove_container_resources(&mut sandbox)?;

            return Ok(());
        }

        // timeout != 0
        let s = self.sandbox.clone();
        let cid2 = cid.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<i32>();

        let handle = tokio::spawn(async move {
            let mut sandbox = s.lock().await;
            if let Some(ctr) = sandbox.get_container(&cid2) {
                ctr.destroy().await.unwrap();
                sandbox.bind_watcher.remove_container(&cid2).await;
                tx.send(1).unwrap();
            };
        });

        if tokio::time::timeout(Duration::from_secs(req.timeout.into()), rx)
            .await
            .is_err()
        {
            return Err(anyhow!(nix::Error::ETIME));
        }

        if handle.await.is_err() {
            return Err(anyhow!(nix::Error::UnknownErrno));
        }

        let s = self.sandbox.clone();
        let mut sandbox = s.lock().await;

        remove_container_resources(&mut sandbox)?;

        Ok(())
    }

    #[instrument]
    async fn do_exec_process(&self, req: protocols::agent::ExecProcessRequest) -> Result<()> {
        let cid = req.container_id.clone();
        let exec_id = req.exec_id.clone();

        info!(sl!(), "do_exec_process cid: {} eid: {}", cid, exec_id);

        let s = self.sandbox.clone();
        let mut sandbox = s.lock().await;

        let mut process = req
            .process
            .into_option()
            .ok_or_else(|| anyhow!(nix::Error::EINVAL))?;

        // Apply any necessary corrections for PCI addresses
        update_env_pci(&mut process.Env, &sandbox.pcimap)?;

        let pipe_size = AGENT_CONFIG.read().await.container_pipe_size;
        let ocip = rustjail::process_grpc_to_oci(&process);
        let p = Process::new(&sl!(), &ocip, exec_id.as_str(), false, pipe_size)?;

        let ctr = sandbox
            .get_container(&cid)
            .ok_or_else(|| anyhow!("Invalid container id"))?;

        ctr.run(p).await?;

        Ok(())
    }

    #[instrument]
    async fn do_signal_process(&self, req: protocols::agent::SignalProcessRequest) -> Result<()> {
        let cid = req.container_id.clone();
        let eid = req.exec_id.clone();
        let s = self.sandbox.clone();

        info!(
            sl!(),
            "signal process";
            "container-id" => cid.clone(),
            "exec-id" => eid.clone(),
        );

        let mut sig: libc::c_int = req.signal as libc::c_int;
        {
            let mut sandbox = s.lock().await;
            let p = sandbox.find_container_process(cid.as_str(), eid.as_str())?;
            // For container initProcess, if it hasn't installed handler for "SIGTERM" signal,
            // it will ignore the "SIGTERM" signal sent to it, thus send it "SIGKILL" signal
            // instead of "SIGTERM" to terminate it.
            if p.init && sig == libc::SIGTERM && !is_signal_handled(p.pid, sig as u32) {
                sig = libc::SIGKILL;
            }
            p.signal(sig)?;
        }

        if eid.is_empty() {
            // eid is empty, signal all the remaining processes in the container cgroup
            info!(
                sl!(),
                "signal all the remaining processes";
                "container-id" => cid.clone(),
                "exec-id" => eid.clone(),
            );

            if let Err(err) = self.freeze_cgroup(&cid, FreezerState::Frozen).await {
                warn!(
                    sl!(),
                    "freeze cgroup failed";
                    "container-id" => cid.clone(),
                    "exec-id" => eid.clone(),
                    "error" => format!("{:?}", err),
                );
            }

            let pids = self.get_pids(&cid).await?;
            for pid in pids.iter() {
                let res = unsafe { libc::kill(*pid, sig) };
                if let Err(err) = Errno::result(res).map(drop) {
                    warn!(
                        sl!(),
                        "signal failed";
                        "container-id" => cid.clone(),
                        "exec-id" => eid.clone(),
                        "pid" => pid,
                        "error" => format!("{:?}", err),
                    );
                }
            }
            if let Err(err) = self.freeze_cgroup(&cid, FreezerState::Thawed).await {
                warn!(
                    sl!(),
                    "unfreeze cgroup failed";
                    "container-id" => cid.clone(),
                    "exec-id" => eid.clone(),
                    "error" => format!("{:?}", err),
                );
            }
        }
        Ok(())
    }

    async fn freeze_cgroup(&self, cid: &str, state: FreezerState) -> Result<()> {
        let s = self.sandbox.clone();
        let mut sandbox = s.lock().await;
        let ctr = sandbox
            .get_container(cid)
            .ok_or_else(|| anyhow!("Invalid container id {}", cid))?;
        let cm = ctr
            .cgroup_manager
            .as_ref()
            .ok_or_else(|| anyhow!("cgroup manager not exist"))?;
        cm.freeze(state)?;
        Ok(())
    }

    async fn get_pids(&self, cid: &str) -> Result<Vec<i32>> {
        let s = self.sandbox.clone();
        let mut sandbox = s.lock().await;
        let ctr = sandbox
            .get_container(cid)
            .ok_or_else(|| anyhow!("Invalid container id {}", cid))?;
        let cm = ctr
            .cgroup_manager
            .as_ref()
            .ok_or_else(|| anyhow!("cgroup manager not exist"))?;
        let pids = cm.get_pids()?;
        Ok(pids)
    }

    #[instrument]
    async fn do_wait_process(
        &self,
        req: protocols::agent::WaitProcessRequest,
    ) -> Result<protocols::agent::WaitProcessResponse> {
        let cid = req.container_id.clone();
        let eid = req.exec_id;
        let s = self.sandbox.clone();
        let mut resp = WaitProcessResponse::new();
        let pid: pid_t;

        let (exit_send, mut exit_recv) = tokio::sync::mpsc::channel(100);

        info!(
            sl!(),
            "wait process";
            "container-id" => cid.clone(),
            "exec-id" => eid.clone()
        );

        let exit_rx = {
            let mut sandbox = s.lock().await;
            let p = sandbox.find_container_process(cid.as_str(), eid.as_str())?;

            p.exit_watchers.push(exit_send);
            pid = p.pid;

            p.exit_rx.clone()
        };

        if let Some(mut exit_rx) = exit_rx {
            info!(sl!(), "cid {} eid {} waiting for exit signal", &cid, &eid);
            while exit_rx.changed().await.is_ok() {}
            info!(sl!(), "cid {} eid {} received exit signal", &cid, &eid);
        }

        let mut sandbox = s.lock().await;
        let ctr = sandbox
            .get_container(&cid)
            .ok_or_else(|| anyhow!("Invalid container id"))?;

        let p = match ctr.processes.get_mut(&pid) {
            Some(p) => p,
            None => {
                // Lost race, pick up exit code from channel
                resp.status = exit_recv
                    .recv()
                    .await
                    .ok_or_else(|| anyhow!("Failed to receive exit code"))?;

                return Ok(resp);
            }
        };

        // need to close all fd
        // ignore errors for some fd might be closed by stream
        p.cleanup_process_stream();

        resp.status = p.exit_code;
        // broadcast exit code to all parallel watchers
        for s in p.exit_watchers.iter_mut() {
            // Just ignore errors in case any watcher quits unexpectedly
            let _ = s.send(p.exit_code).await;
        }

        ctr.processes.remove(&pid);

        Ok(resp)
    }

    async fn do_write_stream(
        &self,
        req: protocols::agent::WriteStreamRequest,
    ) -> Result<protocols::agent::WriteStreamResponse> {
        let cid = req.container_id.clone();
        let eid = req.exec_id.clone();

        let writer = {
            let s = self.sandbox.clone();
            let mut sandbox = s.lock().await;
            let p = sandbox.find_container_process(cid.as_str(), eid.as_str())?;

            // use ptmx io
            if p.term_master.is_some() {
                p.get_writer(StreamType::TermMaster)
            } else {
                // use piped io
                p.get_writer(StreamType::ParentStdin)
            }
        };

        let writer = writer.ok_or_else(|| anyhow!("cannot get writer"))?;
        writer.lock().await.write_all(req.data.as_slice()).await?;

        let mut resp = WriteStreamResponse::new();
        resp.set_len(req.data.len() as u32);

        Ok(resp)
    }

    async fn do_read_stream(
        &self,
        req: protocols::agent::ReadStreamRequest,
        stdout: bool,
    ) -> Result<protocols::agent::ReadStreamResponse> {
        let cid = req.container_id;
        let eid = req.exec_id;

        let mut term_exit_notifier = Arc::new(tokio::sync::Notify::new());
        let reader = {
            let s = self.sandbox.clone();
            let mut sandbox = s.lock().await;

            let p = sandbox.find_container_process(cid.as_str(), eid.as_str())?;

            if p.term_master.is_some() {
                term_exit_notifier = p.term_exit_notifier.clone();
                p.get_reader(StreamType::TermMaster)
            } else if stdout {
                if p.parent_stdout.is_some() {
                    p.get_reader(StreamType::ParentStdout)
                } else {
                    None
                }
            } else {
                p.get_reader(StreamType::ParentStderr)
            }
        };

        if reader.is_none() {
            return Err(anyhow!(nix::Error::EINVAL));
        }

        let reader = reader.ok_or_else(|| anyhow!("cannot get stream reader"))?;

        tokio::select! {
            _ = term_exit_notifier.notified() => {
                Err(anyhow!("eof"))
            }
            v = read_stream(reader, req.len as usize)  => {
                let vector = v?;
                let mut resp = ReadStreamResponse::new();
                resp.set_data(vector);

                Ok(resp)
            }
        }
    }

    // When being passed an image name through a container annotation, merge its
    // corresponding bundle OCI specification into the passed container creation one.
    async fn merge_bundle_oci(&self, container_oci: &mut oci::Spec) -> Result<()> {
        if let Some(image_name) = container_oci
            .annotations
            .get(&ANNO_K8S_IMAGE_NAME.to_string())
        {
            if let Some(container_id) = self.sandbox.clone().lock().await.images.get(image_name) {
                let image_oci_config_path = Path::new(CONTAINER_BASE)
                    .join(container_id)
                    .join(CONFIG_JSON);
                debug!(
                    sl!(),
                    "Image bundle config path: {:?}", image_oci_config_path
                );

                let image_oci =
                    oci::Spec::load(image_oci_config_path.to_str().ok_or_else(|| {
                        anyhow!(
                            "Invalid container image OCI config path {:?}",
                            image_oci_config_path
                        )
                    })?)
                    .context("load image bundle")?;

                if let Some(container_root) = container_oci.root.as_mut() {
                    if let Some(image_root) = image_oci.root.as_ref() {
                        let root_path = Path::new(CONTAINER_BASE)
                            .join(container_id)
                            .join(image_root.path.clone());
                        container_root.path =
                            String::from(root_path.to_str().ok_or_else(|| {
                                anyhow!("Invalid container image root path {:?}", root_path)
                            })?);
                    }
                }

                if let Some(container_process) = container_oci.process.as_mut() {
                    if let Some(image_process) = image_oci.process.as_ref() {
                        merge_oci_process(container_process, image_process);
                    }
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl protocols::agent_ttrpc::AgentService for AgentService {
    async fn create_container(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::CreateContainerRequest,
    ) -> ttrpc::Result<Empty> {
        trace_rpc_call!(ctx, "create_container", req);
        is_allowed!(req);
        match self.do_create_container(req).await {
            Err(e) => Err(ttrpc_error!(ttrpc::Code::INTERNAL, e)),
            Ok(_) => Ok(Empty::new()),
        }
    }

    async fn start_container(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::StartContainerRequest,
    ) -> ttrpc::Result<Empty> {
        trace_rpc_call!(ctx, "start_container", req);
        is_allowed!(req);
        match self.do_start_container(req).await {
            Err(e) => Err(ttrpc_error!(ttrpc::Code::INTERNAL, e)),
            Ok(_) => Ok(Empty::new()),
        }
    }

    async fn remove_container(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::RemoveContainerRequest,
    ) -> ttrpc::Result<Empty> {
        trace_rpc_call!(ctx, "remove_container", req);
        is_allowed!(req);

        match self.do_remove_container(req).await {
            Err(e) => Err(ttrpc_error!(ttrpc::Code::INTERNAL, e)),
            Ok(_) => Ok(Empty::new()),
        }
    }

    async fn exec_process(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::ExecProcessRequest,
    ) -> ttrpc::Result<Empty> {
        trace_rpc_call!(ctx, "exec_process", req);
        is_allowed!(req);
        match self.do_exec_process(req).await {
            Err(e) => Err(ttrpc_error!(ttrpc::Code::INTERNAL, e)),
            Ok(_) => Ok(Empty::new()),
        }
    }

    async fn signal_process(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::SignalProcessRequest,
    ) -> ttrpc::Result<Empty> {
        trace_rpc_call!(ctx, "signal_process", req);
        is_allowed!(req);
        match self.do_signal_process(req).await {
            Err(e) => Err(ttrpc_error!(ttrpc::Code::INTERNAL, e)),
            Ok(_) => Ok(Empty::new()),
        }
    }

    async fn wait_process(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::WaitProcessRequest,
    ) -> ttrpc::Result<WaitProcessResponse> {
        trace_rpc_call!(ctx, "wait_process", req);
        is_allowed!(req);
        self.do_wait_process(req)
            .await
            .map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))
    }

    async fn update_container(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::UpdateContainerRequest,
    ) -> ttrpc::Result<Empty> {
        trace_rpc_call!(ctx, "update_container", req);
        is_allowed!(req);
        let cid = req.container_id.clone();
        let res = req.resources;

        let s = Arc::clone(&self.sandbox);
        let mut sandbox = s.lock().await;

        let ctr = sandbox.get_container(&cid).ok_or_else(|| {
            ttrpc_error!(
                ttrpc::Code::INVALID_ARGUMENT,
                "invalid container id".to_string(),
            )
        })?;

        let resp = Empty::new();

        if let Some(res) = res.as_ref() {
            let oci_res = rustjail::resources_grpc_to_oci(res);
            match ctr.set(oci_res) {
                Err(e) => {
                    return Err(ttrpc_error!(ttrpc::Code::INTERNAL, e));
                }

                Ok(_) => return Ok(resp),
            }
        }

        Ok(resp)
    }

    async fn stats_container(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::StatsContainerRequest,
    ) -> ttrpc::Result<StatsContainerResponse> {
        trace_rpc_call!(ctx, "stats_container", req);
        is_allowed!(req);
        let cid = req.container_id;
        let s = Arc::clone(&self.sandbox);
        let mut sandbox = s.lock().await;

        let ctr = sandbox.get_container(&cid).ok_or_else(|| {
            ttrpc_error!(
                ttrpc::Code::INVALID_ARGUMENT,
                "invalid container id".to_string(),
            )
        })?;

        ctr.stats()
            .map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))
    }

    async fn pause_container(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::PauseContainerRequest,
    ) -> ttrpc::Result<protocols::empty::Empty> {
        trace_rpc_call!(ctx, "pause_container", req);
        is_allowed!(req);
        let cid = req.get_container_id();
        let s = Arc::clone(&self.sandbox);
        let mut sandbox = s.lock().await;

        let ctr = sandbox.get_container(cid).ok_or_else(|| {
            ttrpc_error!(
                ttrpc::Code::INVALID_ARGUMENT,
                "invalid container id".to_string(),
            )
        })?;

        ctr.pause()
            .map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))?;

        Ok(Empty::new())
    }

    async fn resume_container(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::ResumeContainerRequest,
    ) -> ttrpc::Result<protocols::empty::Empty> {
        trace_rpc_call!(ctx, "resume_container", req);
        is_allowed!(req);
        let cid = req.get_container_id();
        let s = Arc::clone(&self.sandbox);
        let mut sandbox = s.lock().await;

        let ctr = sandbox.get_container(cid).ok_or_else(|| {
            ttrpc_error!(
                ttrpc::Code::INVALID_ARGUMENT,
                "invalid container id".to_string(),
            )
        })?;

        ctr.resume()
            .map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))?;

        Ok(Empty::new())
    }

    async fn write_stdin(
        &self,
        _ctx: &TtrpcContext,
        req: protocols::agent::WriteStreamRequest,
    ) -> ttrpc::Result<WriteStreamResponse> {
        is_allowed!(req);
        self.do_write_stream(req)
            .await
            .map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))
    }

    async fn read_stdout(
        &self,
        _ctx: &TtrpcContext,
        req: protocols::agent::ReadStreamRequest,
    ) -> ttrpc::Result<ReadStreamResponse> {
        is_allowed!(req);
        self.do_read_stream(req, true)
            .await
            .map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))
    }

    async fn read_stderr(
        &self,
        _ctx: &TtrpcContext,
        req: protocols::agent::ReadStreamRequest,
    ) -> ttrpc::Result<ReadStreamResponse> {
        is_allowed!(req);
        self.do_read_stream(req, false)
            .await
            .map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))
    }

    async fn close_stdin(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::CloseStdinRequest,
    ) -> ttrpc::Result<Empty> {
        trace_rpc_call!(ctx, "close_stdin", req);
        is_allowed!(req);

        let cid = req.container_id.clone();
        let eid = req.exec_id;
        let s = Arc::clone(&self.sandbox);
        let mut sandbox = s.lock().await;

        let p = sandbox
            .find_container_process(cid.as_str(), eid.as_str())
            .map_err(|e| {
                ttrpc_error!(
                    ttrpc::Code::INVALID_ARGUMENT,
                    format!("invalid argument: {:?}", e),
                )
            })?;

        p.close_stdin();

        Ok(Empty::new())
    }

    async fn tty_win_resize(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::TtyWinResizeRequest,
    ) -> ttrpc::Result<Empty> {
        trace_rpc_call!(ctx, "tty_win_resize", req);
        is_allowed!(req);

        let cid = req.container_id.clone();
        let eid = req.exec_id.clone();
        let s = Arc::clone(&self.sandbox);
        let mut sandbox = s.lock().await;
        let p = sandbox
            .find_container_process(cid.as_str(), eid.as_str())
            .map_err(|e| {
                ttrpc_error!(
                    ttrpc::Code::UNAVAILABLE,
                    format!("invalid argument: {:?}", e),
                )
            })?;

        if let Some(fd) = p.term_master {
            unsafe {
                let win = winsize {
                    ws_row: req.row as c_ushort,
                    ws_col: req.column as c_ushort,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };

                let err = libc::ioctl(fd, TIOCSWINSZ, &win);
                Errno::result(err).map(drop).map_err(|e| {
                    ttrpc_error!(ttrpc::Code::INTERNAL, format!("ioctl error: {:?}", e))
                })?;
            }
        } else {
            return Err(ttrpc_error!(ttrpc::Code::UNAVAILABLE, "no tty".to_string()));
        }

        Ok(Empty::new())
    }

    async fn update_interface(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::UpdateInterfaceRequest,
    ) -> ttrpc::Result<Interface> {
        trace_rpc_call!(ctx, "update_interface", req);
        is_allowed!(req);

        let interface = req.interface.into_option().ok_or_else(|| {
            ttrpc_error!(
                ttrpc::Code::INVALID_ARGUMENT,
                "empty update interface request".to_string(),
            )
        })?;

        self.sandbox
            .lock()
            .await
            .rtnl
            .update_interface(&interface)
            .await
            .map_err(|e| {
                ttrpc_error!(ttrpc::Code::INTERNAL, format!("update interface: {:?}", e))
            })?;

        Ok(interface)
    }

    async fn update_routes(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::UpdateRoutesRequest,
    ) -> ttrpc::Result<Routes> {
        trace_rpc_call!(ctx, "update_routes", req);
        is_allowed!(req);

        let new_routes = req
            .routes
            .into_option()
            .map(|r| r.Routes.into_vec())
            .ok_or_else(|| {
                ttrpc_error!(
                    ttrpc::Code::INVALID_ARGUMENT,
                    "empty update routes request".to_string(),
                )
            })?;

        let mut sandbox = self.sandbox.lock().await;

        sandbox.rtnl.update_routes(new_routes).await.map_err(|e| {
            ttrpc_error!(
                ttrpc::Code::INTERNAL,
                format!("Failed to update routes: {:?}", e),
            )
        })?;

        let list = sandbox.rtnl.list_routes().await.map_err(|e| {
            ttrpc_error!(
                ttrpc::Code::INTERNAL,
                format!("Failed to list routes after update: {:?}", e),
            )
        })?;

        Ok(protocols::agent::Routes {
            Routes: RepeatedField::from_vec(list),
            ..Default::default()
        })
    }

    async fn list_interfaces(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::ListInterfacesRequest,
    ) -> ttrpc::Result<Interfaces> {
        trace_rpc_call!(ctx, "list_interfaces", req);
        is_allowed!(req);

        let list = self
            .sandbox
            .lock()
            .await
            .rtnl
            .list_interfaces()
            .await
            .map_err(|e| {
                ttrpc_error!(
                    ttrpc::Code::INTERNAL,
                    format!("Failed to list interfaces: {:?}", e),
                )
            })?;

        Ok(protocols::agent::Interfaces {
            Interfaces: RepeatedField::from_vec(list),
            ..Default::default()
        })
    }

    async fn list_routes(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::ListRoutesRequest,
    ) -> ttrpc::Result<Routes> {
        trace_rpc_call!(ctx, "list_routes", req);
        is_allowed!(req);

        let list = self
            .sandbox
            .lock()
            .await
            .rtnl
            .list_routes()
            .await
            .map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, format!("list routes: {:?}", e)))?;

        Ok(protocols::agent::Routes {
            Routes: RepeatedField::from_vec(list),
            ..Default::default()
        })
    }

    async fn create_sandbox(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::CreateSandboxRequest,
    ) -> ttrpc::Result<Empty> {
        trace_rpc_call!(ctx, "create_sandbox", req);
        is_allowed!(req);

        {
            let sandbox = self.sandbox.clone();
            let mut s = sandbox.lock().await;

            let _ = fs::remove_dir_all(CONTAINER_BASE);
            let _ = fs::create_dir_all(CONTAINER_BASE);

            s.hostname = req.hostname.clone();
            s.running = true;

            if !req.guest_hook_path.is_empty() {
                let _ = s.add_hooks(&req.guest_hook_path).map_err(|e| {
                    error!(
                        sl!(),
                        "add guest hook {} failed: {:?}", req.guest_hook_path, e
                    );
                });
            }

            if !req.sandbox_id.is_empty() {
                s.id = req.sandbox_id.clone();
            }

            for m in req.kernel_modules.iter() {
                load_kernel_module(m).map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))?;
            }

            s.setup_shared_namespaces()
                .await
                .map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))?;
        }

        match add_storages(sl!(), req.storages.to_vec(), self.sandbox.clone(), None).await {
            Ok(m) => {
                let sandbox = self.sandbox.clone();
                let mut s = sandbox.lock().await;
                s.mounts = m
            }
            Err(e) => return Err(ttrpc_error!(ttrpc::Code::INTERNAL, e)),
        };

        match setup_guest_dns(sl!(), req.dns.to_vec()) {
            Ok(_) => {
                let sandbox = self.sandbox.clone();
                let mut s = sandbox.lock().await;
                let _dns = req
                    .dns
                    .to_vec()
                    .iter()
                    .map(|dns| s.network.set_dns(dns.to_string()));
            }
            Err(e) => return Err(ttrpc_error!(ttrpc::Code::INTERNAL, e)),
        };

        Ok(Empty::new())
    }

    async fn destroy_sandbox(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::DestroySandboxRequest,
    ) -> ttrpc::Result<Empty> {
        trace_rpc_call!(ctx, "destroy_sandbox", req);
        is_allowed!(req);

        let s = Arc::clone(&self.sandbox);
        let mut sandbox = s.lock().await;
        // destroy all containers, clean up, notify agent to exit
        // etc.
        sandbox
            .destroy()
            .await
            .map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))?;
        // Close get_oom_event connection,
        // otherwise it will block the shutdown of ttrpc.
        sandbox.event_tx.take();

        sandbox
            .sender
            .take()
            .ok_or_else(|| {
                ttrpc_error!(
                    ttrpc::Code::INTERNAL,
                    "failed to get sandbox sender channel".to_string(),
                )
            })?
            .send(1)
            .map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))?;

        Ok(Empty::new())
    }

    async fn add_arp_neighbors(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::AddARPNeighborsRequest,
    ) -> ttrpc::Result<Empty> {
        trace_rpc_call!(ctx, "add_arp_neighbors", req);
        is_allowed!(req);

        let neighs = req
            .neighbors
            .into_option()
            .map(|n| n.ARPNeighbors.into_vec())
            .ok_or_else(|| {
                ttrpc_error!(
                    ttrpc::Code::INVALID_ARGUMENT,
                    "empty add arp neighbours request".to_string(),
                )
            })?;

        self.sandbox
            .lock()
            .await
            .rtnl
            .add_arp_neighbors(neighs)
            .await
            .map_err(|e| {
                ttrpc_error!(
                    ttrpc::Code::INTERNAL,
                    format!("Failed to add ARP neighbours: {:?}", e),
                )
            })?;

        Ok(Empty::new())
    }

    async fn online_cpu_mem(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::OnlineCPUMemRequest,
    ) -> ttrpc::Result<Empty> {
        is_allowed!(req);
        let s = Arc::clone(&self.sandbox);
        let sandbox = s.lock().await;
        trace_rpc_call!(ctx, "online_cpu_mem", req);

        sandbox
            .online_cpu_memory(&req)
            .map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))?;

        Ok(Empty::new())
    }

    async fn reseed_random_dev(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::ReseedRandomDevRequest,
    ) -> ttrpc::Result<Empty> {
        trace_rpc_call!(ctx, "reseed_random_dev", req);
        is_allowed!(req);

        random::reseed_rng(req.data.as_slice())
            .map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))?;

        Ok(Empty::new())
    }

    async fn get_guest_details(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::GuestDetailsRequest,
    ) -> ttrpc::Result<GuestDetailsResponse> {
        trace_rpc_call!(ctx, "get_guest_details", req);
        is_allowed!(req);

        info!(sl!(), "get guest details!");
        let mut resp = GuestDetailsResponse::new();
        // to get memory block size
        match get_memory_info(
            req.mem_block_size,
            req.mem_hotplug_probe,
            SYSFS_MEMORY_BLOCK_SIZE_PATH,
            SYSFS_MEMORY_HOTPLUG_PROBE_PATH,
        ) {
            Ok((u, v)) => {
                resp.mem_block_size_bytes = u;
                resp.support_mem_hotplug_probe = v;
            }
            Err(e) => {
                info!(sl!(), "fail to get memory info!");
                return Err(ttrpc_error!(ttrpc::Code::INTERNAL, e));
            }
        }

        // to get agent details
        let detail = get_agent_details();
        resp.agent_details = SingularPtrField::some(detail);

        Ok(resp)
    }

    async fn mem_hotplug_by_probe(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::MemHotplugByProbeRequest,
    ) -> ttrpc::Result<Empty> {
        trace_rpc_call!(ctx, "mem_hotplug_by_probe", req);
        is_allowed!(req);

        do_mem_hotplug_by_probe(&req.memHotplugProbeAddr)
            .map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))?;

        Ok(Empty::new())
    }

    async fn set_guest_date_time(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::SetGuestDateTimeRequest,
    ) -> ttrpc::Result<Empty> {
        trace_rpc_call!(ctx, "set_guest_date_time", req);
        is_allowed!(req);

        do_set_guest_date_time(req.Sec, req.Usec)
            .map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))?;

        Ok(Empty::new())
    }

    async fn copy_file(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::CopyFileRequest,
    ) -> ttrpc::Result<Empty> {
        trace_rpc_call!(ctx, "copy_file", req);
        is_allowed!(req);

        do_copy_file(&req).map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))?;

        Ok(Empty::new())
    }

    async fn get_metrics(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::GetMetricsRequest,
    ) -> ttrpc::Result<Metrics> {
        trace_rpc_call!(ctx, "get_metrics", req);
        is_allowed!(req);

        match get_metrics(&req) {
            Err(e) => Err(ttrpc_error!(ttrpc::Code::INTERNAL, e)),
            Ok(s) => {
                let mut metrics = Metrics::new();
                metrics.set_metrics(s);
                Ok(metrics)
            }
        }
    }

    async fn get_oom_event(
        &self,
        _ctx: &TtrpcContext,
        req: protocols::agent::GetOOMEventRequest,
    ) -> ttrpc::Result<OOMEvent> {
        is_allowed!(req);
        let sandbox = self.sandbox.clone();
        let s = sandbox.lock().await;
        let event_rx = &s.event_rx.clone();
        let mut event_rx = event_rx.lock().await;
        drop(s);
        drop(sandbox);

        if let Some(container_id) = event_rx.recv().await {
            info!(sl!(), "get_oom_event return {}", &container_id);

            let mut resp = OOMEvent::new();
            resp.container_id = container_id;

            return Ok(resp);
        }

        Err(ttrpc_error!(ttrpc::Code::INTERNAL, ""))
    }

    async fn get_volume_stats(
        &self,
        ctx: &TtrpcContext,
        req: VolumeStatsRequest,
    ) -> ttrpc::Result<VolumeStatsResponse> {
        trace_rpc_call!(ctx, "get_volume_stats", req);
        is_allowed!(req);

        info!(sl!(), "get volume stats!");
        let mut resp = VolumeStatsResponse::new();

        let mut condition = VolumeCondition::new();

        match File::open(&req.volume_guest_path) {
            Ok(_) => {
                condition.abnormal = false;
                condition.message = String::from("OK");
            }
            Err(e) => {
                info!(sl!(), "failed to open the volume");
                return Err(ttrpc_error!(ttrpc::Code::INTERNAL, e));
            }
        };

        let mut usage_vec = Vec::new();

        // to get volume capacity stats
        get_volume_capacity_stats(&req.volume_guest_path)
            .map(|u| usage_vec.push(u))
            .map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))?;

        // to get volume inode stats
        get_volume_inode_stats(&req.volume_guest_path)
            .map(|u| usage_vec.push(u))
            .map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))?;

        resp.usage = RepeatedField::from_vec(usage_vec);
        resp.volume_condition = SingularPtrField::some(condition);
        Ok(resp)
    }

    async fn add_swap(
        &self,
        ctx: &TtrpcContext,
        req: protocols::agent::AddSwapRequest,
    ) -> ttrpc::Result<Empty> {
        trace_rpc_call!(ctx, "add_swap", req);
        is_allowed!(req);

        do_add_swap(&self.sandbox, &req)
            .await
            .map_err(|e| ttrpc_error!(ttrpc::Code::INTERNAL, e))?;

        Ok(Empty::new())
    }
}

#[derive(Clone)]
struct HealthService;

#[async_trait]
impl protocols::health_ttrpc::Health for HealthService {
    async fn check(
        &self,
        _ctx: &TtrpcContext,
        _req: protocols::health::CheckRequest,
    ) -> ttrpc::Result<HealthCheckResponse> {
        let mut resp = HealthCheckResponse::new();
        resp.set_status(HealthCheckResponse_ServingStatus::SERVING);

        Ok(resp)
    }

    async fn version(
        &self,
        _ctx: &TtrpcContext,
        req: protocols::health::CheckRequest,
    ) -> ttrpc::Result<VersionCheckResponse> {
        info!(sl!(), "version {:?}", req);
        let mut rep = protocols::health::VersionCheckResponse::new();
        rep.agent_version = AGENT_VERSION.to_string();
        rep.grpc_version = API_VERSION.to_string();

        Ok(rep)
    }
}

fn get_memory_info(
    block_size: bool,
    hotplug: bool,
    block_size_path: &str,
    hotplug_probe_path: &str,
) -> Result<(u64, bool)> {
    let mut size: u64 = 0;
    let mut plug: bool = false;
    if block_size {
        match fs::read_to_string(block_size_path) {
            Ok(v) => {
                if v.is_empty() {
                    warn!(sl!(), "file {} is empty", block_size_path);
                    return Err(anyhow!(ERR_INVALID_BLOCK_SIZE));
                }

                size = u64::from_str_radix(v.trim(), 16).map_err(|_| {
                    warn!(sl!(), "failed to parse the str {} to hex", size);
                    anyhow!(ERR_INVALID_BLOCK_SIZE)
                })?;
            }
            Err(e) => {
                warn!(sl!(), "memory block size error: {:?}", e.kind());
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(anyhow!(e));
                }
            }
        }
    }

    if hotplug {
        match stat::stat(hotplug_probe_path) {
            Ok(_) => plug = true,
            Err(e) => {
                warn!(sl!(), "hotplug memory error: {:?}", e);
                match e {
                    nix::Error::ENOENT => plug = false,
                    _ => return Err(anyhow!(e)),
                }
            }
        }
    }

    Ok((size, plug))
}

fn get_volume_capacity_stats(path: &str) -> Result<VolumeUsage> {
    let mut usage = VolumeUsage::new();

    let s = System::new();
    for disk in s.disks() {
        if let Some(v) = disk.name().to_str() {
            if v.to_string().eq(path) {
                usage.available = disk.available_space();
                usage.total = disk.total_space();
                usage.used = usage.total - usage.available;
                usage.unit = VolumeUsage_Unit::BYTES; // bytes
                break;
            }
        } else {
            return Err(anyhow!(nix::Error::EINVAL));
        }
    }

    Ok(usage)
}

fn get_volume_inode_stats(path: &str) -> Result<VolumeUsage> {
    let mut usage = VolumeUsage::new();

    let s = System::new();
    for disk in s.disks() {
        if let Some(v) = disk.name().to_str() {
            if v.to_string().eq(path) {
                let meta = fs::metadata(disk.mount_point())?;
                let inode = meta.ino();
                usage.used = inode;
                usage.unit = VolumeUsage_Unit::INODES;
                break;
            }
        } else {
            return Err(anyhow!(nix::Error::EINVAL));
        }
    }

    Ok(usage)
}

pub fn have_seccomp() -> bool {
    if cfg!(feature = "seccomp") {
        return true;
    }

    false
}

fn get_agent_details() -> AgentDetails {
    let mut detail = AgentDetails::new();

    detail.set_version(AGENT_VERSION.to_string());
    detail.set_supports_seccomp(have_seccomp());
    detail.init_daemon = unistd::getpid() == Pid::from_raw(1);

    detail.device_handlers = RepeatedField::new();
    detail.storage_handlers = RepeatedField::from_vec(
        STORAGE_HANDLER_LIST
            .to_vec()
            .iter()
            .map(|x| x.to_string())
            .collect(),
    );

    detail
}

async fn read_stream(reader: Arc<Mutex<ReadHalf<PipeStream>>>, l: usize) -> Result<Vec<u8>> {
    let mut content = vec![0u8; l];

    let mut reader = reader.lock().await;
    let len = reader.read(&mut content).await?;
    content.resize(len, 0);

    if len == 0 {
        return Err(anyhow!("read meet eof"));
    }

    Ok(content)
}

pub fn start(s: Arc<Mutex<Sandbox>>, server_address: &str) -> Result<TtrpcServer> {
    let agent_service = Box::new(AgentService { sandbox: s.clone() })
        as Box<dyn protocols::agent_ttrpc::AgentService + Send + Sync>;

    let agent_worker = Arc::new(agent_service);

    let health_service =
        Box::new(HealthService {}) as Box<dyn protocols::health_ttrpc::Health + Send + Sync>;
    let health_worker = Arc::new(health_service);

    let image_service = Box::new(image_rpc::ImageService::new(s))
        as Box<dyn protocols::image_ttrpc::Image + Send + Sync>;

    let agent_service = protocols::agent_ttrpc::create_agent_service(agent_worker);

    let health_service = protocols::health_ttrpc::create_health(health_worker);

    let image_service = protocols::image_ttrpc::create_image(Arc::new(image_service));

    let server = TtrpcServer::new()
        .bind(server_address)?
        .register_service(agent_service)
        .register_service(health_service)
        .register_service(image_service);

    info!(sl!(), "ttRPC server started"; "address" => server_address);

    Ok(server)
}

// This function updates the container namespaces configuration based on the
// sandbox information. When the sandbox is created, it can be setup in a way
// that all containers will share some specific namespaces. This is the agent
// responsibility to create those namespaces so that they can be shared across
// several containers.
// If the sandbox has not been setup to share namespaces, then we assume all
// containers will be started in their own new namespace.
// The value of a.sandbox.sharedPidNs.path will always override the namespace
// path set by the spec, since we will always ignore it. Indeed, it makes no
// sense to rely on the namespace path provided by the host since namespaces
// are different inside the guest.
fn update_container_namespaces(
    sandbox: &Sandbox,
    spec: &mut Spec,
    sandbox_pidns: bool,
) -> Result<()> {
    let linux = spec
        .linux
        .as_mut()
        .ok_or_else(|| anyhow!("Spec didn't container linux field"))?;

    let namespaces = linux.namespaces.as_mut_slice();
    for namespace in namespaces.iter_mut() {
        if namespace.r#type == NSTYPEIPC {
            namespace.path = sandbox.shared_ipcns.path.clone();
            continue;
        }
        if namespace.r#type == NSTYPEUTS {
            namespace.path = sandbox.shared_utsns.path.clone();
            continue;
        }
    }
    // update pid namespace
    let mut pid_ns = LinuxNamespace {
        r#type: NSTYPEPID.to_string(),
        ..Default::default()
    };

    // Use shared pid ns if useSandboxPidns has been set in either
    // the create_sandbox request or create_container request.
    // Else set this to empty string so that a new pid namespace is
    // created for the container.
    if sandbox_pidns {
        if let Some(ref pidns) = &sandbox.sandbox_pidns {
            pid_ns.path = String::from(pidns.path.as_str());
        } else {
            return Err(anyhow!("failed to get sandbox pidns"));
        }
    }

    linux.namespaces.push(pid_ns);
    Ok(())
}

fn append_guest_hooks(s: &Sandbox, oci: &mut Spec) -> Result<()> {
    if let Some(ref guest_hooks) = s.hooks {
        let mut hooks = oci.hooks.take().unwrap_or_default();
        hooks.prestart.append(&mut guest_hooks.prestart.clone());
        hooks.poststart.append(&mut guest_hooks.poststart.clone());
        hooks.poststop.append(&mut guest_hooks.poststop.clone());
        oci.hooks = Some(hooks);
    }

    Ok(())
}

// Check is the container process installed the
// handler for specific signal.
fn is_signal_handled(pid: pid_t, signum: u32) -> bool {
    let sig_mask: u64 = 1u64 << (signum - 1);
    let file_name = format!("/proc/{}/status", pid);

    // Open the file in read-only mode (ignoring errors).
    let file = match File::open(&file_name) {
        Ok(f) => f,
        Err(_) => {
            warn!(sl!(), "failed to open file {}\n", file_name);
            return false;
        }
    };

    let reader = BufReader::new(file);

    // Read the file line by line using the lines() iterator from std::io::BufRead.
    for (_index, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(_) => {
                warn!(sl!(), "failed to read file {}\n", file_name);
                return false;
            }
        };
        if line.starts_with("SigCgt:") {
            let mask_vec: Vec<&str> = line.split(':').collect();
            if mask_vec.len() != 2 {
                warn!(sl!(), "parse the SigCgt field failed\n");
                return false;
            }
            let sig_cgt_str = mask_vec[1];
            let sig_cgt_mask = match u64::from_str_radix(sig_cgt_str, 16) {
                Ok(h) => h,
                Err(_) => {
                    warn!(sl!(), "failed to parse the str {} to hex\n", sig_cgt_str);
                    return false;
                }
            };

            return (sig_cgt_mask & sig_mask) == sig_mask;
        }
    }
    false
}

fn do_mem_hotplug_by_probe(addrs: &[u64]) -> Result<()> {
    for addr in addrs.iter() {
        fs::write(SYSFS_MEMORY_HOTPLUG_PROBE_PATH, format!("{:#X}", *addr))?;
    }
    Ok(())
}

fn do_set_guest_date_time(sec: i64, usec: i64) -> Result<()> {
    let tv = libc::timeval {
        tv_sec: sec,
        tv_usec: usec,
    };

    let ret = unsafe {
        libc::settimeofday(
            &tv as *const libc::timeval,
            std::ptr::null::<libc::timezone>(),
        )
    };

    Errno::result(ret).map(drop)?;

    Ok(())
}

fn do_copy_file(req: &CopyFileRequest) -> Result<()> {
    let path = PathBuf::from(req.path.as_str());

    if !path.starts_with(CONTAINER_BASE) {
        return Err(anyhow!(nix::Error::EINVAL));
    }

    let parent = path.parent();

    let dir = if let Some(parent) = parent {
        parent.to_path_buf()
    } else {
        PathBuf::from("/")
    };

    fs::create_dir_all(&dir).or_else(|e| {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(e);
        }

        Ok(())
    })?;

    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(req.dir_mode))?;

    let mut tmpfile = path.clone();
    tmpfile.set_extension("tmp");

    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&tmpfile)?;

    file.write_all_at(req.data.as_slice(), req.offset as u64)?;
    let st = stat::stat(&tmpfile)?;

    if st.st_size != req.file_size {
        return Ok(());
    }

    file.set_permissions(std::fs::Permissions::from_mode(req.file_mode))?;

    unistd::chown(
        &tmpfile,
        Some(Uid::from_raw(req.uid as u32)),
        Some(Gid::from_raw(req.gid as u32)),
    )?;

    fs::rename(tmpfile, path)?;

    Ok(())
}

async fn do_add_swap(sandbox: &Arc<Mutex<Sandbox>>, req: &AddSwapRequest) -> Result<()> {
    let mut slots = Vec::new();
    for slot in &req.PCIPath {
        slots.push(pci::SlotFn::new(*slot, 0)?);
    }
    let pcipath = pci::Path::new(slots)?;
    let dev_name = get_virtio_blk_pci_device_name(sandbox, &pcipath).await?;

    let c_str = CString::new(dev_name)?;
    let ret = unsafe { libc::swapon(c_str.as_ptr() as *const c_char, 0) };
    if ret != 0 {
        return Err(anyhow!(
            "libc::swapon get error {}",
            io::Error::last_os_error()
        ));
    }

    Ok(())
}

// Setup container bundle under CONTAINER_BASE, which is cleaned up
// before removing a container.
// - bundle path is /<CONTAINER_BASE>/<cid>/
// - config.json at /<CONTAINER_BASE>/<cid>/config.json
// - container rootfs bind mounted at /<CONTAINER_BASE>/<cid>/rootfs
// - modify container spec root to point to /<CONTAINER_BASE>/<cid>/rootfs
fn setup_bundle(cid: &str, spec: &mut Spec) -> Result<PathBuf> {
    let spec_root = if let Some(sr) = &spec.root {
        sr
    } else {
        return Err(anyhow!(nix::Error::EINVAL));
    };

    let spec_root_path = Path::new(&spec_root.path);

    let bundle_path = Path::new(CONTAINER_BASE).join(cid);
    let config_path = bundle_path.join(CONFIG_JSON);
    let rootfs_path = bundle_path.join("rootfs");

    let rootfs_exists = Path::new(&rootfs_path).exists();
    info!(
        sl!(),
        "The rootfs_path is {:?} and exists: {}", rootfs_path, rootfs_exists
    );

    if !rootfs_exists {
        fs::create_dir_all(&rootfs_path)?;
        baremount(
            spec_root_path,
            &rootfs_path,
            "bind",
            MsFlags::MS_BIND,
            "",
            &sl!(),
        )?;
    }

    let rootfs_path_name = rootfs_path
        .to_str()
        .ok_or_else(|| anyhow!("failed to convert rootfs to unicode"))?
        .to_string();

    spec.root = Some(Root {
        path: rootfs_path_name,
        readonly: spec_root.readonly,
    });

    let _ = spec.save(
        config_path
            .to_str()
            .ok_or_else(|| anyhow!("cannot convert path to unicode"))?,
    );

    let olddir = unistd::getcwd().context("cannot getcwd")?;
    unistd::chdir(
        bundle_path
            .to_str()
            .ok_or_else(|| anyhow!("cannot convert bundle path to unicode"))?,
    )?;

    Ok(olddir)
}

fn load_kernel_module(module: &protocols::agent::KernelModule) -> Result<()> {
    if module.name.is_empty() {
        return Err(anyhow!("Kernel module name is empty"));
    }

    info!(
        sl!(),
        "load_kernel_module {}: {:?}", module.name, module.parameters
    );

    let mut args = vec!["-v".to_string(), module.name.clone()];

    if module.parameters.len() > 0 {
        args.extend(module.parameters.to_vec())
    }

    let output = Command::new(MODPROBE_PATH)
        .args(args.as_slice())
        .stdout(Stdio::piped())
        .output()?;

    let status = output.status;
    if status.success() {
        return Ok(());
    }

    match status.code() {
        Some(code) => {
            let std_out = String::from_utf8_lossy(&output.stdout);
            let std_err = String::from_utf8_lossy(&output.stderr);
            let msg = format!(
                "load_kernel_module return code: {} stdout:{} stderr:{}",
                code, std_out, std_err
            );
            Err(anyhow!(msg))
        }
        None => Err(anyhow!("Process terminated by signal")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::agent_ttrpc::AgentService as _;
    use oci::{Hook, Hooks};
    use tempfile::tempdir;
    use ttrpc::{r#async::TtrpcContext, MessageHeader};

    // Parameters:
    //
    // 1: expected Result
    // 2: actual Result
    // 3: string used to identify the test on error
    macro_rules! assert_result {
        ($expected_result:expr, $actual_result:expr, $msg:expr) => {
            if $expected_result.is_ok() {
                let expected_level = $expected_result.as_ref().unwrap();
                let actual_level = $actual_result.unwrap();
                assert!(*expected_level == actual_level, "{}", $msg);
            } else {
                let expected_error = $expected_result.as_ref().unwrap_err();
                let expected_error_msg = format!("{:?}", expected_error);

                if let Err(actual_error) = $actual_result {
                    let actual_error_msg = format!("{:?}", actual_error);

                    assert!(expected_error_msg == actual_error_msg, "{}", $msg);
                } else {
                    assert!(expected_error_msg == "expected error, got OK", "{}", $msg);
                }
            }
        };
    }

    fn mk_ttrpc_context() -> TtrpcContext {
        TtrpcContext {
            fd: -1,
            mh: MessageHeader::default(),
            metadata: std::collections::HashMap::new(),
            timeout_nano: 0,
        }
    }

    #[test]
    fn test_load_kernel_module() {
        let mut m = protocols::agent::KernelModule {
            name: "module_not_exists".to_string(),
            ..Default::default()
        };

        // case 1: module not exists
        let result = load_kernel_module(&m);
        assert!(result.is_err(), "load module should failed");

        // case 2: module name is empty
        m.name = "".to_string();
        let result = load_kernel_module(&m);
        assert!(result.is_err(), "load module should failed");

        // case 3: normal module.
        // normally this module should eixsts...
        m.name = "bridge".to_string();
        let result = load_kernel_module(&m);
        assert!(result.is_ok(), "load module should success");
    }

    #[tokio::test]
    async fn test_append_guest_hooks() {
        let logger = slog::Logger::root(slog::Discard, o!());
        let mut s = Sandbox::new(&logger).unwrap();
        s.hooks = Some(Hooks {
            prestart: vec![Hook {
                path: "foo".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let mut oci = Spec {
            ..Default::default()
        };
        append_guest_hooks(&s, &mut oci).unwrap();
        assert_eq!(s.hooks, oci.hooks);
    }

    #[tokio::test]
    async fn test_update_interface() {
        let logger = slog::Logger::root(slog::Discard, o!());
        let sandbox = Sandbox::new(&logger).unwrap();

        let agent_service = Box::new(AgentService {
            sandbox: Arc::new(Mutex::new(sandbox)),
        });

        let req = protocols::agent::UpdateInterfaceRequest::default();
        let ctx = mk_ttrpc_context();

        let result = agent_service.update_interface(&ctx, req).await;

        assert!(result.is_err(), "expected update interface to fail");
    }

    #[tokio::test]
    async fn test_update_routes() {
        let logger = slog::Logger::root(slog::Discard, o!());
        let sandbox = Sandbox::new(&logger).unwrap();

        let agent_service = Box::new(AgentService {
            sandbox: Arc::new(Mutex::new(sandbox)),
        });

        let req = protocols::agent::UpdateRoutesRequest::default();
        let ctx = mk_ttrpc_context();

        let result = agent_service.update_routes(&ctx, req).await;

        assert!(result.is_err(), "expected update routes to fail");
    }

    #[tokio::test]
    async fn test_add_arp_neighbors() {
        let logger = slog::Logger::root(slog::Discard, o!());
        let sandbox = Sandbox::new(&logger).unwrap();

        let agent_service = Box::new(AgentService {
            sandbox: Arc::new(Mutex::new(sandbox)),
        });

        let req = protocols::agent::AddARPNeighborsRequest::default();
        let ctx = mk_ttrpc_context();

        let result = agent_service.add_arp_neighbors(&ctx, req).await;

        assert!(result.is_err(), "expected add arp neighbors to fail");
    }

    #[tokio::test]
    async fn test_get_memory_info() {
        #[derive(Debug)]
        struct TestData<'a> {
            // if None is provided, no file will be generated, else the data in the Option will populate the file
            block_size_data: Option<&'a str>,

            hotplug_probe_data: bool,
            get_block_size: bool,
            get_hotplug: bool,
            result: Result<(u64, bool)>,
        }

        let tests = &[
            TestData {
                block_size_data: Some("10000000"),
                hotplug_probe_data: true,
                get_block_size: true,
                get_hotplug: true,
                result: Ok((268435456, true)),
            },
            TestData {
                block_size_data: Some("100"),
                hotplug_probe_data: false,
                get_block_size: true,
                get_hotplug: true,
                result: Ok((256, false)),
            },
            TestData {
                block_size_data: None,
                hotplug_probe_data: false,
                get_block_size: true,
                get_hotplug: true,
                result: Ok((0, false)),
            },
            TestData {
                block_size_data: Some(""),
                hotplug_probe_data: false,
                get_block_size: true,
                get_hotplug: false,
                result: Err(anyhow!(ERR_INVALID_BLOCK_SIZE)),
            },
            TestData {
                block_size_data: Some("-1"),
                hotplug_probe_data: false,
                get_block_size: true,
                get_hotplug: false,
                result: Err(anyhow!(ERR_INVALID_BLOCK_SIZE)),
            },
            TestData {
                block_size_data: Some("    "),
                hotplug_probe_data: false,
                get_block_size: true,
                get_hotplug: false,
                result: Err(anyhow!(ERR_INVALID_BLOCK_SIZE)),
            },
            TestData {
                block_size_data: Some("some data"),
                hotplug_probe_data: false,
                get_block_size: true,
                get_hotplug: false,
                result: Err(anyhow!(ERR_INVALID_BLOCK_SIZE)),
            },
            TestData {
                block_size_data: Some("some data"),
                hotplug_probe_data: true,
                get_block_size: false,
                get_hotplug: false,
                result: Ok((0, false)),
            },
            TestData {
                block_size_data: Some("100"),
                hotplug_probe_data: true,
                get_block_size: false,
                get_hotplug: false,
                result: Ok((0, false)),
            },
            TestData {
                block_size_data: Some("100"),
                hotplug_probe_data: true,
                get_block_size: false,
                get_hotplug: true,
                result: Ok((0, true)),
            },
        ];

        for (i, d) in tests.iter().enumerate() {
            let msg = format!("test[{}]: {:?}", i, d);

            let dir = tempdir().expect("failed to make tempdir");
            let block_size_path = dir.path().join("block_size_bytes");
            let hotplug_probe_path = dir.path().join("probe");

            if let Some(block_size_data) = d.block_size_data {
                fs::write(&block_size_path, block_size_data).unwrap();
            }
            if d.hotplug_probe_data {
                fs::write(&hotplug_probe_path, []).unwrap();
            }

            let result = get_memory_info(
                d.get_block_size,
                d.get_hotplug,
                block_size_path.to_str().unwrap(),
                hotplug_probe_path.to_str().unwrap(),
            );

            let msg = format!("{}, result: {:?}", msg, result);

            assert_result!(d.result, result, msg);
        }
    }

    #[tokio::test]
    async fn test_verify_cid() {
        #[derive(Debug)]
        struct TestData<'a> {
            id: &'a str,
            expect_error: bool,
        }

        let tests = &[
            TestData {
                // Cannot be blank
                id: "",
                expect_error: true,
            },
            TestData {
                // Cannot be a space
                id: " ",
                expect_error: true,
            },
            TestData {
                // Must start with an alphanumeric
                id: ".",
                expect_error: true,
            },
            TestData {
                // Must start with an alphanumeric
                id: "-",
                expect_error: true,
            },
            TestData {
                // Must start with an alphanumeric
                id: "_",
                expect_error: true,
            },
            TestData {
                // Must start with an alphanumeric
                id: " a",
                expect_error: true,
            },
            TestData {
                // Must start with an alphanumeric
                id: ".a",
                expect_error: true,
            },
            TestData {
                // Must start with an alphanumeric
                id: "-a",
                expect_error: true,
            },
            TestData {
                // Must start with an alphanumeric
                id: "_a",
                expect_error: true,
            },
            TestData {
                // Must start with an alphanumeric
                id: "..",
                expect_error: true,
            },
            TestData {
                // Too short
                id: "a",
                expect_error: true,
            },
            TestData {
                // Too short
                id: "z",
                expect_error: true,
            },
            TestData {
                // Too short
                id: "A",
                expect_error: true,
            },
            TestData {
                // Too short
                id: "Z",
                expect_error: true,
            },
            TestData {
                // Too short
                id: "0",
                expect_error: true,
            },
            TestData {
                // Too short
                id: "9",
                expect_error: true,
            },
            TestData {
                // Must start with an alphanumeric
                id: "-1",
                expect_error: true,
            },
            TestData {
                id: "/",
                expect_error: true,
            },
            TestData {
                id: "a/",
                expect_error: true,
            },
            TestData {
                id: "a/../",
                expect_error: true,
            },
            TestData {
                id: "../a",
                expect_error: true,
            },
            TestData {
                id: "../../a",
                expect_error: true,
            },
            TestData {
                id: "../../../a",
                expect_error: true,
            },
            TestData {
                id: "foo/../bar",
                expect_error: true,
            },
            TestData {
                id: "foo bar",
                expect_error: true,
            },
            TestData {
                id: "a.",
                expect_error: false,
            },
            TestData {
                id: "a..",
                expect_error: false,
            },
            TestData {
                id: "aa",
                expect_error: false,
            },
            TestData {
                id: "aa.",
                expect_error: false,
            },
            TestData {
                id: "hello..world",
                expect_error: false,
            },
            TestData {
                id: "hello/../world",
                expect_error: true,
            },
            TestData {
                id: "aa1245124sadfasdfgasdga.",
                expect_error: false,
            },
            TestData {
                id: "aAzZ0123456789_.-",
                expect_error: false,
            },
            TestData {
                id: "abcdefghijklmnopqrstuvwxyz0123456789.-_",
                expect_error: false,
            },
            TestData {
                id: "0123456789abcdefghijklmnopqrstuvwxyz.-_",
                expect_error: false,
            },
            TestData {
                id: " abcdefghijklmnopqrstuvwxyz0123456789.-_",
                expect_error: true,
            },
            TestData {
                id: ".abcdefghijklmnopqrstuvwxyz0123456789.-_",
                expect_error: true,
            },
            TestData {
                id: "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.-_",
                expect_error: false,
            },
            TestData {
                id: "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ.-_",
                expect_error: false,
            },
            TestData {
                id: " ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.-_",
                expect_error: true,
            },
            TestData {
                id: ".ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.-_",
                expect_error: true,
            },
            TestData {
                id: "/a/b/c",
                expect_error: true,
            },
            TestData {
                id: "a/b/c",
                expect_error: true,
            },
            TestData {
                id: "foo/../../../etc/passwd",
                expect_error: true,
            },
            TestData {
                id: "../../../../../../etc/motd",
                expect_error: true,
            },
            TestData {
                id: "/etc/passwd",
                expect_error: true,
            },
        ];

        for (i, d) in tests.iter().enumerate() {
            let msg = format!("test[{}]: {:?}", i, d);

            let result = verify_cid(d.id);

            let msg = format!("{}, result: {:?}", msg, result);

            if result.is_ok() {
                assert!(!d.expect_error, "{}", msg);
            } else {
                assert!(d.expect_error, "{}", msg);
            }
        }
    }
}
