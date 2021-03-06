// Copyright 2021 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use super::*;
use bee_message::prelude::MilestoneIndex;
use chronicle_common::Wrapper;
use chronicle_storage::access::SyncRecord;

#[async_trait::async_trait]
impl<H: ChronicleBrokerScope> EventLoop<BrokerHandle<H>> for Archiver {
    async fn event_loop(
        &mut self,
        status: Result<(), Need>,
        _supervisor: &mut Option<BrokerHandle<H>>,
    ) -> Result<(), Need> {
        status?;
        let mut next;
        if let Ok(index) = self.oneshot.take().unwrap().await {
            next = index;
        } else {
            return Err(Need::Abort);
        }
        info!(
            "Archiver will write ahead log files for new incoming data starting: {}",
            next
        );
        self.service.update_status(ServiceStatus::Running);
        let event = BrokerEvent::Children(BrokerChild::Archiver(self.service.clone(), status));
        let _ = _supervisor
            .as_mut()
            .expect("Archiver Expected BrokerHandle")
            .send(event);
        while let Some(event) = self.inbox.rx.recv().await {
            match event {
                ArchiverEvent::Close(milestone_index) => {
                    // to prevent overlap, we ensure to only close syncer milestone_index when it's less than next
                    if milestone_index < next {
                        self.close_log_file(milestone_index).await.map_err(|e| {
                            error!("{}", e);
                            Need::Abort
                        })?;
                    }
                }
                ArchiverEvent::MilestoneData(milestone_data, opt_upper_limit) => {
                    info!(
                        "Archiver received milestone data for index: {}, upper_ms_limit: {:?}",
                        milestone_data.milestone_index(),
                        opt_upper_limit
                    );
                    // check if it belongs to new incoming data
                    if !milestone_data.created_by().eq(&CreatedBy::Syncer) {
                        self.milestones_data.push(Ascending::new(milestone_data));
                        while let Some(ms_data) = self.milestones_data.pop() {
                            let ms_index = ms_data.milestone_index();
                            if next.eq(&ms_index) {
                                self.handle_milestone_data(ms_data.into_inner(), opt_upper_limit)
                                    .await
                                    .map_err(|e| {
                                        error!("{}", e);
                                        Need::Abort
                                    })?;
                                next += 1;
                            } else if ms_index > next {
                                // Safety check to prevent potential rare race condition
                                // check if we buffered too much.
                                if self.milestones_data.len() > self.solidifiers_count as usize {
                                    error!("Identified gap in the new incoming data: {}..{}", next, ms_index);
                                    // Close the file which we're unable atm to append on top.
                                    self.close_log_file(next).await.map_err(|e| {
                                        error!("{}", e);
                                        Need::Abort
                                    })?;
                                    // this supposed to create new file
                                    self.handle_milestone_data(ms_data.into_inner(), opt_upper_limit)
                                        .await
                                        .map_err(|e| {
                                            error!("{}", e);
                                            Need::Abort
                                        })?;
                                    // reset next
                                    next = ms_index + 1;
                                } else {
                                    self.milestones_data.push(ms_data);
                                    break;
                                }
                            } else {
                                warn!("Expected: {}, Dropping milestone_data: {}, as the syncer will eventually fill it up",next, ms_index);
                            }
                        }
                    } else {
                        // to prevent overlap, we ensure to only handle syncer milestone_data when it's less than next
                        if milestone_data.milestone_index() < next {
                            // handle syncer milestone data;
                            self.handle_milestone_data(milestone_data, opt_upper_limit)
                                .await
                                .map_err(|e| {
                                    error!("{}", e);
                                    Need::Abort
                                })?;
                            // it overlaps with the incoming flow.
                        } else if milestone_data.milestone_index() == next {
                            // we handle the milestone_data from syncer as Incoming without upper_ms_limit
                            self.handle_milestone_data(milestone_data, None).await.map_err(|e| {
                                error!("{}", e);
                                Need::Abort
                            })?;
                            next += 1;
                        } else {
                            // we received a futuristic milestone_data from syncer.
                            self.milestones_data.push(Ascending::new(milestone_data));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

impl Archiver {
    async fn close_log_file(&mut self, milestone_index: u32) -> anyhow::Result<()> {
        if let Some((i, log_file)) = self
            .logs
            .iter_mut()
            .enumerate()
            .find(|(_, log)| log.to_ms_index == milestone_index)
        {
            Self::finish_log_file(log_file, &self.dir_path).await?;
            // remove finished log file
            let log_file = self.logs.remove(i);
            self.push_to_processed(log_file);
        };
        Ok(())
    }
    async fn handle_milestone_data(
        &mut self,
        milestone_data: MilestoneData,
        mut opt_upper_limit: Option<u32>,
    ) -> anyhow::Result<()> {
        let milestone_index = milestone_data.milestone_index();
        let mut milestone_data_json = serde_json::to_string(&milestone_data).unwrap();
        milestone_data_json.push('\n');
        let milestone_data_line: Vec<u8> = milestone_data_json.into();
        // check the logs files to find if any has already existing log file
        if let Some(log_file) = self
            .logs
            .iter_mut()
            .find(|log| log.to_ms_index == milestone_index && log.upper_ms_limit > milestone_index)
        {
            // append milestone data to the log file if the file_size still less than max limit
            if (milestone_data_line.len() as u64) + log_file.len() < self.max_log_size {
                Self::append(
                    log_file,
                    &milestone_data_line,
                    milestone_index,
                    &self.keyspace,
                    self.retries_per_query,
                )
                .await?;
                // check if now the log_file reached an upper limit to finish the file
                if log_file.upper_ms_limit == log_file.to_ms_index {
                    self.cleanup.push(log_file.from_ms_index);
                    Self::finish_log_file(log_file, &self.dir_path).await?;
                }
            } else {
                // push it into cleanup
                self.cleanup.push(log_file.from_ms_index);
                // Finish it;
                Self::finish_log_file(log_file, &self.dir_path).await?;
                info!(
                    "{} hits filesize limit: {} bytes, contains: {} milestones data",
                    log_file.filename,
                    log_file.len(),
                    log_file.milestones_range()
                );
                // check if the milestone_index already belongs to an existing processed logs
                let not_processed = !self.processed.iter().any(|r| r.contains(&milestone_index));
                if not_processed {
                    // create new file
                    info!(
                        "Creating new log file starting from milestone index: {}",
                        milestone_index
                    );
                    opt_upper_limit.replace(log_file.upper_ms_limit);
                    self.create_and_append(milestone_index, &milestone_data_line, opt_upper_limit)
                        .await?;
                }
            }
        } else {
            // check if the milestone_index already belongs to an existing processed files/ranges;
            if !self.processed.iter().any(|r| r.contains(&milestone_index)) {
                info!(
                    "Creating new log file starting from milestone index: {}",
                    milestone_index
                );
                self.create_and_append(milestone_index, &milestone_data_line, opt_upper_limit)
                    .await?;
            };
        };
        // remove finished log file
        while let Some(from_ms_index) = self.cleanup.pop() {
            let i = self
                .logs
                .iter()
                .position(|item| item.from_ms_index == from_ms_index)
                .unwrap();
            let log_file = self.logs.remove(i);
            self.push_to_processed(log_file);
        }
        Ok(())
    }
    async fn create_and_append(
        &mut self,
        milestone_index: u32,
        milestone_data_line: &Vec<u8>,
        opt_upper_limit: Option<u32>,
    ) -> anyhow::Result<()> {
        let mut log_file = LogFile::create(&self.dir_path, milestone_index, opt_upper_limit).await?;
        Self::append(
            &mut log_file,
            milestone_data_line,
            milestone_index,
            &self.keyspace,
            self.retries_per_query,
        )
        .await?;
        // check if we hit an upper_ms_limit, as this is possible when the log_file only needs 1 milestone data.
        if log_file.upper_ms_limit == log_file.to_ms_index {
            // finish it
            Self::finish_log_file(&mut log_file, &self.dir_path).await?;
            // add it to processed
            self.push_to_processed(log_file);
        } else {
            // push it to the active log files
            self.logs.push(log_file);
            self.logs.sort_by(|a, b| a.from_ms_index.cmp(&b.from_ms_index));
            // iterate in reverse
            let mut log_files = self.logs.iter_mut().rev();
            // extract the last log_file
            if let Some(mut prev_log) = log_files.next() {
                // iterate in reverse to adjust the upper_ms_limit
                while let Some(l) = log_files.next() {
                    if l.upper_ms_limit > prev_log.from_ms_index {
                        l.upper_ms_limit = prev_log.from_ms_index;
                    }
                    // check if the L file needs to be closed
                    if l.upper_ms_limit.eq(&l.to_ms_index) && !l.finished {
                        // push it into cleanup to get removed and pushed to processed
                        self.cleanup.push(l.from_ms_index);
                        // finish the file
                        Self::finish_log_file(l, &self.dir_path).await?;
                    }

                    prev_log = l;
                }
            }
        }
        Ok(())
    }
    fn push_to_processed(&mut self, log_file: LogFile) {
        let r = std::ops::Range {
            start: log_file.from_ms_index,
            end: log_file.to_ms_index,
        };
        info!("Logged Range: {:?}", r);
        self.processed.push(r);
        self.processed.sort_by(|a, b| b.start.cmp(&a.start));
    }
    async fn append(
        log_file: &mut LogFile,
        milestone_data_line: &Vec<u8>,
        ms_index: u32,
        keyspace: &ChronicleKeyspace,
        retries_per_query: usize,
    ) -> anyhow::Result<()> {
        log_file.append_line(&milestone_data_line).await?;
        // insert into the DB, without caring about the response
        let sync_key = chronicle_common::Synckey;
        let synced_record = SyncRecord::new(MilestoneIndex(ms_index), None, Some(0));
        keyspace
            .insert(&sync_key, &synced_record)
            .consistency(Consistency::One)
            .build()?
            .send_local(InsertWorker::boxed(
                keyspace.clone(),
                sync_key,
                synced_record,
                retries_per_query,
            ));
        Ok(())
    }
    async fn finish_log_file(log_file: &mut LogFile, dir_path: &PathBuf) -> anyhow::Result<()> {
        log_file.finish(dir_path).await?;
        log_file.set_finished();
        info!(
            "Finished {}.part, LogFile: {}to{}.log",
            log_file.from_ms_index, log_file.from_ms_index, log_file.to_ms_index
        );
        Ok(())
    }
}
