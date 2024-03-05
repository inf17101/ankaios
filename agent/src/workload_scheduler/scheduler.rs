// Copyright (c) 2024 Elektrobit Automotive GmbH
//
// This program and the accompanying materials are made available under the
// terms of the Apache License, Version 2.0 which is available at
// https://www.apache.org/licenses/LICENSE-2.0.
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS, WITHOUT
// WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied. See the
// License for the specific language governing permissions and limitations
// under the License.
//
// SPDX-License-Identifier: Apache-2.0

#[cfg_attr(test, mockall_double::double)]
use crate::workload_scheduler::dependency_state_validator::DependencyStateValidator;

use common::{
    objects::{DeletedWorkload, ExecutionState, WorkloadInstanceName, WorkloadSpec, WorkloadState},
    std_extensions::IllegalStateResult,
    to_server_interface::{ToServerInterface, ToServerSender},
};
use std::collections::HashMap;

#[cfg_attr(test, mockall_double::double)]
use crate::parameter_storage::ParameterStorage;
use crate::workload_operation::{WorkloadOperation, WorkloadOperations};

#[cfg(test)]
use mockall::automock;

#[derive(Debug, Clone, PartialEq)]
enum PendingEntry {
    Create(WorkloadSpec),
    Delete(DeletedWorkload),
    UpdateCreate(WorkloadSpec, DeletedWorkload),
    UpdateDelete(WorkloadSpec, DeletedWorkload),
}

type WorkloadOperationQueue = HashMap<String, Box<dyn IPendingEntry + Send + Sync + 'static>>;

pub enum QueueState {
    Same,
    NewUpdateCreateState(
        Box<dyn IPendingEntry + Send + Sync + 'static>,
        WorkloadOperation,
    ),
    Ready(WorkloadOperation),
}

pub trait IPendingEntry {
    fn next_state(&self, workload_state_db: &ParameterStorage) -> QueueState;
    fn instance_name(&self) -> WorkloadInstanceName;
}

struct PendingCreate {
    workload_spec: WorkloadSpec,
}

impl PendingCreate {
    pub fn new(workload_spec: WorkloadSpec) -> Self {
        PendingCreate { workload_spec }
    }
}

impl IPendingEntry for PendingCreate {
    fn next_state(&self, workload_state_db: &ParameterStorage) -> QueueState {
        if DependencyStateValidator::create_fulfilled(&self.workload_spec, workload_state_db) {
            QueueState::Ready(WorkloadOperation::Create(self.workload_spec.clone()))
        } else {
            QueueState::Same
        }
    }

    fn instance_name(&self) -> WorkloadInstanceName {
        self.workload_spec.instance_name.clone()
    }
}

struct PendingDelete {
    deleted_workload: DeletedWorkload,
}

impl PendingDelete {
    pub fn new(deleted_workload: DeletedWorkload) -> Self {
        PendingDelete { deleted_workload }
    }
}

impl IPendingEntry for PendingDelete {
    fn next_state(&self, workload_state_db: &ParameterStorage) -> QueueState {
        if DependencyStateValidator::delete_fulfilled(&self.deleted_workload, workload_state_db) {
            QueueState::Ready(WorkloadOperation::Delete(self.deleted_workload.clone()))
        } else {
            QueueState::Same
        }
    }

    fn instance_name(&self) -> WorkloadInstanceName {
        self.deleted_workload.instance_name.clone()
    }
}

struct PendingUpdateCreate {
    new_workload_spec: WorkloadSpec,
    deleted_workload: DeletedWorkload,
}

impl PendingUpdateCreate {
    pub fn new(new_workload_spec: WorkloadSpec, deleted_workload: DeletedWorkload) -> Self {
        PendingUpdateCreate {
            new_workload_spec,
            deleted_workload,
        }
    }
}

impl IPendingEntry for PendingUpdateCreate {
    fn next_state(&self, workload_state_db: &ParameterStorage) -> QueueState {
        if DependencyStateValidator::create_fulfilled(&self.new_workload_spec, workload_state_db) {
            QueueState::Ready(WorkloadOperation::Update(
                self.new_workload_spec.clone(),
                self.deleted_workload.clone(),
            ))
        } else {
            QueueState::Same
        }
    }

    fn instance_name(&self) -> WorkloadInstanceName {
        self.new_workload_spec.instance_name.clone()
    }
}

struct PendingUpdateDelete {
    new_workload_spec: WorkloadSpec,
    deleted_workload: DeletedWorkload,
}

impl PendingUpdateDelete {
    pub fn new(new_workload_spec: WorkloadSpec, deleted_workload: DeletedWorkload) -> Self {
        PendingUpdateDelete {
            new_workload_spec,
            deleted_workload,
        }
    }
}

impl IPendingEntry for PendingUpdateDelete {
    fn next_state(&self, workload_state_db: &ParameterStorage) -> QueueState {
        let create_fulfilled =
            DependencyStateValidator::create_fulfilled(&self.new_workload_spec, workload_state_db);

        let delete_fulfilled =
            DependencyStateValidator::delete_fulfilled(&self.deleted_workload, workload_state_db);

        if create_fulfilled && delete_fulfilled {
            // dependencies for create and delete are fulfilled, the update can be done immediately
            return QueueState::Ready(WorkloadOperation::Update(
                self.new_workload_spec.clone(),
                self.deleted_workload.clone(),
            ));
        }

        if delete_fulfilled {
            /* For an update with pending create dependencies but fulfilled delete dependencies
            the delete can be done immediately but the create must wait in the queue.
            If the create dependencies are already fulfilled the update must wait until the
            old workload is deleted (AT_MOST_ONCE default update strategy) */

            /* once the delete conditions are fulfilled the pending update delete is
            transformed into a pending create since the current update strategy is at most once.
            We notify a pending create state. */
            QueueState::NewUpdateCreateState(
                Box::new(PendingUpdateCreate::new(
                    self.new_workload_spec.clone(),
                    self.deleted_workload.clone(),
                )),
                WorkloadOperation::UpdateDeleteOnly(self.deleted_workload.clone()),
            )
        } else {
            QueueState::Same
        }
    }

    fn instance_name(&self) -> WorkloadInstanceName {
        self.new_workload_spec.instance_name.clone()
    }
}

pub struct WorkloadScheduler {
    queue: WorkloadOperationQueue,
    workload_state_sender: ToServerSender,
}

#[cfg_attr(test, automock)]
impl WorkloadScheduler {
    pub fn new(workload_state_tx: ToServerSender) -> Self {
        WorkloadScheduler {
            queue: WorkloadOperationQueue::new(),
            workload_state_sender: workload_state_tx,
        }
    }

    async fn report_pending_create_state(&self, instance_name: &WorkloadInstanceName) {
        self.workload_state_sender
            .update_workload_state(vec![WorkloadState {
                instance_name: instance_name.clone(),
                execution_state: ExecutionState::waiting_to_start(),
            }])
            .await
            .unwrap_or_illegal_state();
    }

    async fn report_pending_delete_state(&self, instance_name: &WorkloadInstanceName) {
        self.workload_state_sender
            .update_workload_state(vec![WorkloadState {
                instance_name: instance_name.clone(),
                execution_state: ExecutionState::waiting_to_stop(),
            }])
            .await
            .unwrap_or_illegal_state();
    }

    pub async fn enqueue_filtered_workload_operations(
        &mut self,
        new_workload_operations: WorkloadOperations,
        workload_state_db: &ParameterStorage,
    ) -> WorkloadOperations {
        let mut ready_workload_operations = WorkloadOperations::new();
        for workload_operation in new_workload_operations {
            match workload_operation {
                WorkloadOperation::Create(new_workload_spec) => {
                    self.queue.insert(
                        new_workload_spec.instance_name.workload_name().to_owned(),
                        Box::new(PendingCreate::new(new_workload_spec)),
                    );
                }
                WorkloadOperation::Update(new_workload_spec, deleted_workload) => {
                    let pending_update = Box::new(PendingUpdateDelete::new(
                        new_workload_spec.clone(),
                        deleted_workload.clone(),
                    ));

                    match pending_update.next_state(workload_state_db) {
                        QueueState::Same => {
                            self.report_pending_delete_state(&deleted_workload.instance_name)
                                .await;

                            self.queue.insert(
                                new_workload_spec.instance_name.workload_name().to_owned(),
                                Box::new(PendingUpdateDelete::new(
                                    new_workload_spec,
                                    deleted_workload,
                                )),
                            );
                        }
                        QueueState::NewUpdateCreateState(
                            pending_update_create,
                            ready_delete_operation,
                        ) => {
                            self.report_pending_create_state(
                                &pending_update_create.instance_name(),
                            )
                            .await;
                            self.queue.insert(
                                pending_update_create
                                    .instance_name()
                                    .workload_name()
                                    .to_owned(),
                                pending_update_create,
                            );

                            ready_workload_operations.push(ready_delete_operation);
                        }
                        QueueState::Ready(workload_operation) => {
                            ready_workload_operations.push(workload_operation)
                        }
                    }
                }
                WorkloadOperation::Delete(deleted_workload) => {
                    self.report_pending_delete_state(&deleted_workload.instance_name)
                        .await;
                    self.queue.insert(
                        deleted_workload.instance_name.workload_name().to_owned(),
                        Box::new(PendingDelete::new(deleted_workload)),
                    );
                }
                WorkloadOperation::UpdateDeleteOnly(_) => {
                    log::warn!("Skip UpdateDeleteOnly. This shall never be enqueued.")
                }
            };
        }

        // extend with existing pending update entries of the queue if their dependencies are fulfilled now
        ready_workload_operations.extend(self.next_workload_operations(workload_state_db).await);
        ready_workload_operations
    }

    pub async fn next_workload_operations(
        &mut self,
        workload_state_db: &ParameterStorage,
    ) -> WorkloadOperations {
        // clear the whole queue without deallocating memory
        let existing_entries: WorkloadOperationQueue = self.queue.drain().collect();

        // return ready workload operations and enqueue still pending workload operations again
        let mut ready_workload_operations = WorkloadOperations::new();

        for (workload_name, pending_operation) in existing_entries {
            match pending_operation.next_state(workload_state_db) {
                QueueState::Same => {
                    self.queue.insert(workload_name, pending_operation);
                }
                QueueState::NewUpdateCreateState(pending_update_create, ready_delete_operation) => {
                    self.report_pending_create_state(&pending_update_create.instance_name())
                        .await;
                    self.queue.insert(workload_name, pending_update_create);
                    ready_workload_operations.push(ready_delete_operation);
                }
                QueueState::Ready(workload_operation) => {
                    ready_workload_operations.push(workload_operation)
                }
            }
        }

        ready_workload_operations
    }
}

//////////////////////////////////////////////////////////////////////////////
//                 ########  #######    #########  #########                //
//                    ##     ##        ##             ##                    //
//                    ##     #####     #########      ##                    //
//                    ##     ##                ##     ##                    //
//                    ##     #######   #########      ##                    //
//////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod tests {
    use common::{
        commands::UpdateWorkloadState,
        objects::{
            generate_test_workload_spec, generate_test_workload_spec_with_param,
            generate_test_workload_state_with_workload_spec, ExecutionState, WorkloadState,
        },
        test_utils::generate_test_deleted_workload,
        to_server_interface::ToServer,
    };
    use tokio::sync::mpsc::channel;

    use super::WorkloadScheduler;
    use crate::{
        parameter_storage::MockParameterStorage,
        workload_operation::WorkloadOperation,
        workload_scheduler::{
            dependency_state_validator::MockDependencyStateValidator, scheduler::PendingEntry,
        },
    };

    const AGENT_A: &str = "agent_A";
    const WORKLOAD_NAME_1: &str = "workload_1";
    const RUNTIME: &str = "runtime";

    #[tokio::test]
    async fn utest_enqueue_and_report_workload_state_for_pending_create() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, mut workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_context =
            MockDependencyStateValidator::create_fulfilled_context();
        mock_dependency_state_validator_context
            .expect()
            .return_const(false);

        let pending_workload = generate_test_workload_spec_with_param(
            AGENT_A.to_owned(),
            WORKLOAD_NAME_1.to_owned(),
            RUNTIME.to_owned(),
        );

        let workload_operations = vec![WorkloadOperation::Create(pending_workload.clone())];

        let ready_workload_operations = workload_scheduler
            .enqueue_filtered_workload_operations(
                workload_operations,
                &MockParameterStorage::default(),
            )
            .await;

        let expected_workload_state = generate_test_workload_state_with_workload_spec(
            &pending_workload.clone(),
            ExecutionState::waiting_to_start(),
        );

        assert_eq!(
            Ok(Some(ToServer::UpdateWorkloadState(UpdateWorkloadState {
                workload_states: vec![expected_workload_state]
            }))),
            tokio::time::timeout(
                tokio::time::Duration::from_millis(100),
                workload_state_receiver.recv()
            )
            .await
        );

        assert!(workload_scheduler
            .queue
            .contains_key(pending_workload.instance_name.workload_name()));

        assert!(ready_workload_operations.is_empty());
    }

    #[tokio::test]
    async fn utest_no_enqueue_and_report_for_ready_create() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, mut workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_context =
            MockDependencyStateValidator::create_fulfilled_context();
        mock_dependency_state_validator_context
            .expect()
            .return_const(true);

        let ready_workload = generate_test_workload_spec_with_param(
            AGENT_A.to_owned(),
            WORKLOAD_NAME_1.to_owned(),
            RUNTIME.to_owned(),
        );

        let workload_operations = vec![WorkloadOperation::Create(ready_workload.clone())];

        let ready_workload_operations = workload_scheduler
            .enqueue_filtered_workload_operations(
                workload_operations,
                &MockParameterStorage::default(),
            )
            .await;

        assert_eq!(
            vec![WorkloadOperation::Create(ready_workload)],
            ready_workload_operations
        );

        assert!(workload_scheduler.queue.is_empty());
        assert!(workload_state_receiver.try_recv().is_err());
    }

    #[tokio::test]
    #[should_panic]
    async fn utest_report_pending_create_state_closed_receiver() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, workload_state_receiver) = channel(1);
        let workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        drop(workload_state_receiver);

        let pending_workload = generate_test_workload_spec();
        workload_scheduler
            .report_pending_create_state(&pending_workload)
            .await;
    }

    #[tokio::test]
    async fn utest_enqueue_and_report_workload_state_for_pending_delete() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, mut workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_context =
            MockDependencyStateValidator::delete_fulfilled_context();
        mock_dependency_state_validator_context
            .expect()
            .return_const(false);

        let pending_deleted_workload =
            generate_test_deleted_workload(AGENT_A.to_owned(), WORKLOAD_NAME_1.to_owned());

        let workload_operations = vec![WorkloadOperation::Delete(pending_deleted_workload.clone())];
        let ready_workload_operations = workload_scheduler
            .enqueue_filtered_workload_operations(
                workload_operations,
                &MockParameterStorage::default(),
            )
            .await;

        assert!(ready_workload_operations.is_empty());

        assert_eq!(
            Some(&PendingEntry::Delete(pending_deleted_workload.clone())),
            workload_scheduler
                .queue
                .get(pending_deleted_workload.instance_name.workload_name())
        );

        let expected_workload_state = WorkloadState {
            instance_name: pending_deleted_workload.instance_name,
            execution_state: ExecutionState::waiting_to_stop(),
        };

        assert_eq!(
            Ok(Some(ToServer::UpdateWorkloadState(UpdateWorkloadState {
                workload_states: vec![expected_workload_state]
            }))),
            tokio::time::timeout(
                tokio::time::Duration::from_millis(100),
                workload_state_receiver.recv()
            )
            .await
        );
    }

    #[tokio::test]
    async fn utest_no_enqueue_and_report_workload_state_for_ready_delete() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, mut workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_context =
            MockDependencyStateValidator::delete_fulfilled_context();
        mock_dependency_state_validator_context
            .expect()
            .return_const(true);

        let ready_deleted_workload =
            generate_test_deleted_workload(AGENT_A.to_owned(), WORKLOAD_NAME_1.to_owned());

        let workload_operations = vec![WorkloadOperation::Delete(ready_deleted_workload.clone())];
        let ready_workload_operations = workload_scheduler
            .enqueue_filtered_workload_operations(
                workload_operations,
                &MockParameterStorage::default(),
            )
            .await;

        assert_eq!(
            vec![WorkloadOperation::Delete(ready_deleted_workload)],
            ready_workload_operations
        );

        assert!(workload_scheduler.queue.is_empty());

        assert!(workload_state_receiver.try_recv().is_err());
    }

    #[tokio::test]
    #[should_panic]
    async fn utest_report_pending_delete_state_closed_receiver() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, workload_state_receiver) = channel(1);
        let workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        drop(workload_state_receiver);

        let pending_workload =
            generate_test_deleted_workload(AGENT_A.to_owned(), WORKLOAD_NAME_1.to_owned());

        workload_scheduler
            .report_pending_delete_state(&pending_workload)
            .await;
    }

    #[tokio::test]
    async fn utest_enqueue_and_report_workload_state_for_pending_update_delete_at_most_once() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, mut workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_create_context =
            MockDependencyStateValidator::create_fulfilled_context();
        mock_dependency_state_validator_create_context
            .expect()
            .return_const(true);

        let mock_dependency_state_validator_delete_context =
            MockDependencyStateValidator::delete_fulfilled_context();
        mock_dependency_state_validator_delete_context
            .expect()
            .return_const(false);

        let ready_new_workload = generate_test_workload_spec_with_param(
            AGENT_A.to_owned(),
            WORKLOAD_NAME_1.to_owned(),
            RUNTIME.to_owned(),
        );

        let pending_deleted_workload = generate_test_deleted_workload(
            ready_new_workload.instance_name.agent_name().to_owned(),
            ready_new_workload.instance_name.workload_name().to_owned(),
        );

        let workload_operations = vec![WorkloadOperation::Update(
            ready_new_workload.clone(),
            pending_deleted_workload.clone(),
        )];
        let ready_workload_operations = workload_scheduler
            .enqueue_filtered_workload_operations(
                workload_operations,
                &MockParameterStorage::default(),
            )
            .await;

        assert!(ready_workload_operations.is_empty());

        assert_eq!(
            Some(&PendingEntry::UpdateDelete(
                ready_new_workload.clone(),
                pending_deleted_workload.clone()
            )),
            workload_scheduler
                .queue
                .get(pending_deleted_workload.instance_name.workload_name())
        );

        let expected_workload_state = WorkloadState {
            instance_name: pending_deleted_workload.instance_name,
            execution_state: ExecutionState::waiting_to_stop(),
        };

        assert_eq!(
            Ok(Some(ToServer::UpdateWorkloadState(UpdateWorkloadState {
                workload_states: vec![expected_workload_state]
            }))),
            tokio::time::timeout(
                tokio::time::Duration::from_millis(100),
                workload_state_receiver.recv()
            )
            .await
        );
    }

    #[tokio::test]
    async fn utest_enqueue_and_report_workload_state_for_pending_update_at_most_once() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, mut workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_create_context =
            MockDependencyStateValidator::create_fulfilled_context();
        mock_dependency_state_validator_create_context
            .expect()
            .return_const(false);

        let mock_dependency_state_validator_delete_context =
            MockDependencyStateValidator::delete_fulfilled_context();
        mock_dependency_state_validator_delete_context
            .expect()
            .return_const(false);

        let ready_new_workload = generate_test_workload_spec_with_param(
            AGENT_A.to_owned(),
            WORKLOAD_NAME_1.to_owned(),
            RUNTIME.to_owned(),
        );

        let pending_deleted_workload = generate_test_deleted_workload(
            ready_new_workload.instance_name.agent_name().to_owned(),
            ready_new_workload.instance_name.workload_name().to_owned(),
        );

        let workload_operations = vec![WorkloadOperation::Update(
            ready_new_workload.clone(),
            pending_deleted_workload.clone(),
        )];
        let ready_workload_operations = workload_scheduler
            .enqueue_filtered_workload_operations(
                workload_operations,
                &MockParameterStorage::default(),
            )
            .await;

        assert!(ready_workload_operations.is_empty());

        assert_eq!(
            Some(&PendingEntry::UpdateDelete(
                ready_new_workload.clone(),
                pending_deleted_workload.clone()
            )),
            workload_scheduler
                .queue
                .get(pending_deleted_workload.instance_name.workload_name())
        );

        let expected_workload_state = WorkloadState {
            instance_name: pending_deleted_workload.instance_name,
            execution_state: ExecutionState::waiting_to_stop(),
        };

        assert_eq!(
            Ok(Some(ToServer::UpdateWorkloadState(UpdateWorkloadState {
                workload_states: vec![expected_workload_state]
            }))),
            tokio::time::timeout(
                tokio::time::Duration::from_millis(100),
                workload_state_receiver.recv()
            )
            .await
        );
    }

    #[tokio::test]
    async fn utest_enqueue_and_report_workload_state_for_pending_update_create_at_most_once() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, mut workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_create_context =
            MockDependencyStateValidator::create_fulfilled_context();
        mock_dependency_state_validator_create_context
            .expect()
            .return_const(false);

        let mock_dependency_state_validator_delete_context =
            MockDependencyStateValidator::delete_fulfilled_context();
        mock_dependency_state_validator_delete_context
            .expect()
            .return_const(true);

        let pending_new_workload = generate_test_workload_spec_with_param(
            AGENT_A.to_owned(),
            WORKLOAD_NAME_1.to_owned(),
            RUNTIME.to_owned(),
        );

        let ready_deleted_workload = generate_test_deleted_workload(
            pending_new_workload.instance_name.agent_name().to_owned(),
            pending_new_workload
                .instance_name
                .workload_name()
                .to_owned(),
        );

        let workload_operations = vec![WorkloadOperation::Update(
            pending_new_workload.clone(),
            ready_deleted_workload.clone(),
        )];

        workload_scheduler
            .enqueue_filtered_workload_operations(
                workload_operations,
                &MockParameterStorage::default(),
            )
            .await;

        assert_eq!(
            Some(&PendingEntry::UpdateCreate(
                pending_new_workload.clone(),
                ready_deleted_workload.clone()
            )),
            workload_scheduler
                .queue
                .get(pending_new_workload.instance_name.workload_name())
        );

        let expected_workload_state = WorkloadState {
            instance_name: pending_new_workload.instance_name,
            execution_state: ExecutionState::waiting_to_start(),
        };

        assert_eq!(
            Ok(Some(ToServer::UpdateWorkloadState(UpdateWorkloadState {
                workload_states: vec![expected_workload_state]
            }))),
            tokio::time::timeout(
                tokio::time::Duration::from_millis(100),
                workload_state_receiver.recv()
            )
            .await
        );
    }

    #[tokio::test]
    async fn utest_immediate_delete_for_pending_update_create_at_most_once() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, _workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_create_context =
            MockDependencyStateValidator::create_fulfilled_context();
        mock_dependency_state_validator_create_context
            .expect()
            .return_const(false);

        let mock_dependency_state_validator_delete_context =
            MockDependencyStateValidator::delete_fulfilled_context();
        mock_dependency_state_validator_delete_context
            .expect()
            .return_const(true);

        let pending_new_workload = generate_test_workload_spec_with_param(
            AGENT_A.to_owned(),
            WORKLOAD_NAME_1.to_owned(),
            RUNTIME.to_owned(),
        );

        let ready_deleted_workload = generate_test_deleted_workload(
            pending_new_workload.instance_name.agent_name().to_owned(),
            pending_new_workload
                .instance_name
                .workload_name()
                .to_owned(),
        );

        let workload_operations = vec![WorkloadOperation::Update(
            pending_new_workload,
            ready_deleted_workload.clone(),
        )];

        let ready_workload_operations = workload_scheduler
            .enqueue_filtered_workload_operations(
                workload_operations,
                &MockParameterStorage::default(),
            )
            .await;

        assert_eq!(
            vec![WorkloadOperation::UpdateDeleteOnly(ready_deleted_workload)],
            ready_workload_operations
        );
    }

    #[tokio::test]
    async fn utest_no_enqueue_and_report_pending_state_on_fulfilled_update_at_most_once() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, mut workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_create_context =
            MockDependencyStateValidator::create_fulfilled_context();
        mock_dependency_state_validator_create_context
            .expect()
            .return_const(true);

        let mock_dependency_state_validator_delete_context =
            MockDependencyStateValidator::delete_fulfilled_context();
        mock_dependency_state_validator_delete_context
            .expect()
            .return_const(true);

        let ready_new_workload = generate_test_workload_spec_with_param(
            AGENT_A.to_owned(),
            WORKLOAD_NAME_1.to_owned(),
            RUNTIME.to_owned(),
        );

        let ready_deleted_workload = generate_test_deleted_workload(
            ready_new_workload.instance_name.agent_name().to_owned(),
            ready_new_workload.instance_name.workload_name().to_owned(),
        );

        let workload_operations = vec![WorkloadOperation::Update(
            ready_new_workload.clone(),
            ready_deleted_workload.clone(),
        )];
        let ready_workload_operations = workload_scheduler
            .enqueue_filtered_workload_operations(
                workload_operations,
                &MockParameterStorage::default(),
            )
            .await;

        assert_eq!(
            vec![WorkloadOperation::Update(
                ready_new_workload,
                ready_deleted_workload
            )],
            ready_workload_operations
        );

        assert!(workload_scheduler.queue.is_empty());

        assert!(workload_state_receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn utest_enqueue_filtered_workload_operations_get_next_ready_workload_operations() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, _workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_create_context =
            MockDependencyStateValidator::create_fulfilled_context();
        mock_dependency_state_validator_create_context
            .expect()
            .once()
            .return_const(true);

        let ready_new_workload = generate_test_workload_spec_with_param(
            AGENT_A.to_owned(),
            WORKLOAD_NAME_1.to_owned(),
            RUNTIME.to_owned(),
        );

        let ready_deleted_workload = generate_test_deleted_workload(
            ready_new_workload.instance_name.agent_name().to_owned(),
            ready_new_workload.instance_name.workload_name().to_owned(),
        );

        let workload_operations = vec![];

        workload_scheduler.queue.insert(
            ready_new_workload.instance_name.workload_name().to_owned(),
            PendingEntry::UpdateCreate(ready_new_workload.clone(), ready_deleted_workload.clone()),
        );

        let ready_workload_operations = workload_scheduler
            .enqueue_filtered_workload_operations(
                workload_operations,
                &MockParameterStorage::default(),
            )
            .await;

        assert_eq!(
            vec![WorkloadOperation::Update(
                ready_new_workload,
                ready_deleted_workload
            )],
            ready_workload_operations
        );

        assert!(workload_scheduler.queue.is_empty());
    }

    #[tokio::test]
    async fn utest_enqueue_filtered_workload_operations_ignore_update_delete_only_workload_operations(
    ) {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, _workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let ready_deleted_workload =
            generate_test_deleted_workload(AGENT_A.to_owned(), WORKLOAD_NAME_1.to_owned());

        let workload_operations = vec![WorkloadOperation::UpdateDeleteOnly(
            ready_deleted_workload.clone(),
        )];

        let ready_workload_operations = workload_scheduler
            .enqueue_filtered_workload_operations(
                workload_operations,
                &MockParameterStorage::default(),
            )
            .await;

        assert!(ready_workload_operations.is_empty());

        assert!(workload_scheduler.queue.is_empty());
    }

    #[tokio::test]
    async fn utest_next_workload_operations_enqueue_pending_update_create_on_delete_fulfilled_update(
    ) {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, _workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_create_context =
            MockDependencyStateValidator::create_fulfilled_context();
        mock_dependency_state_validator_create_context
            .expect()
            .once()
            .return_const(false);

        let mock_dependency_state_validator_delete_context =
            MockDependencyStateValidator::delete_fulfilled_context();
        mock_dependency_state_validator_delete_context
            .expect()
            .return_const(true);

        let new_workload_spec = generate_test_workload_spec_with_param(
            AGENT_A.to_owned(),
            WORKLOAD_NAME_1.to_owned(),
            RUNTIME.to_owned(),
        );

        let instance_name_new_workload = new_workload_spec.instance_name.clone();

        let ready_deleted_workload = generate_test_deleted_workload(
            instance_name_new_workload.agent_name().to_owned(),
            instance_name_new_workload.workload_name().to_owned(),
        );

        workload_scheduler.queue.insert(
            instance_name_new_workload.workload_name().to_owned(),
            PendingEntry::UpdateDelete(new_workload_spec.clone(), ready_deleted_workload.clone()),
        );

        let ready_workload_operations = workload_scheduler
            .next_workload_operations(&MockParameterStorage::default())
            .await;

        assert_eq!(
            vec![WorkloadOperation::UpdateDeleteOnly(
                ready_deleted_workload.clone()
            )],
            ready_workload_operations
        );

        assert_eq!(
            Some(&PendingEntry::UpdateCreate(
                new_workload_spec,
                ready_deleted_workload
            )),
            workload_scheduler
                .queue
                .get(instance_name_new_workload.workload_name())
        );
    }

    #[tokio::test]
    async fn utest_next_workload_operations_report_pending_create_on_delete_fulfilled_update() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, mut workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_create_context =
            MockDependencyStateValidator::create_fulfilled_context();
        mock_dependency_state_validator_create_context
            .expect()
            .once()
            .return_const(false);

        let mock_dependency_state_validator_delete_context =
            MockDependencyStateValidator::delete_fulfilled_context();
        mock_dependency_state_validator_delete_context
            .expect()
            .return_const(true);

        let new_workload_spec = generate_test_workload_spec_with_param(
            AGENT_A.to_owned(),
            WORKLOAD_NAME_1.to_owned(),
            RUNTIME.to_owned(),
        );

        let instance_name_new_workload = new_workload_spec.instance_name.clone();

        let ready_deleted_workload = generate_test_deleted_workload(
            instance_name_new_workload.agent_name().to_owned(),
            instance_name_new_workload.workload_name().to_owned(),
        );

        workload_scheduler.queue.insert(
            instance_name_new_workload.workload_name().to_owned(),
            PendingEntry::UpdateDelete(new_workload_spec.clone(), ready_deleted_workload.clone()),
        );

        workload_scheduler
            .next_workload_operations(&MockParameterStorage::default())
            .await;

        let expected_workload_state = WorkloadState {
            instance_name: instance_name_new_workload,
            execution_state: ExecutionState::waiting_to_start(),
        };

        assert_eq!(
            Ok(Some(ToServer::UpdateWorkloadState(UpdateWorkloadState {
                workload_states: vec![expected_workload_state]
            }))),
            tokio::time::timeout(
                tokio::time::Duration::from_millis(100),
                workload_state_receiver.recv()
            )
            .await
        );
    }

    #[tokio::test]
    async fn utest_next_workload_operations_keep_pending_delete_in_queue() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, _workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_delete_context =
            MockDependencyStateValidator::delete_fulfilled_context();
        mock_dependency_state_validator_delete_context
            .expect()
            .return_const(false);

        let pending_deleted_workload =
            generate_test_deleted_workload(AGENT_A.to_owned(), WORKLOAD_NAME_1.to_owned());

        let instance_name_deleted_workload = pending_deleted_workload.instance_name.clone();

        workload_scheduler.queue.insert(
            instance_name_deleted_workload.workload_name().to_owned(),
            PendingEntry::Delete(pending_deleted_workload.clone()),
        );

        let ready_workload_operations = workload_scheduler
            .next_workload_operations(&MockParameterStorage::default())
            .await;

        assert!(ready_workload_operations.is_empty());

        assert!(workload_scheduler
            .queue
            .contains_key(instance_name_deleted_workload.workload_name()));
    }

    #[tokio::test]
    async fn utest_next_workload_operations_no_report_pending_delete_on_reenqueue() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, mut workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_delete_context =
            MockDependencyStateValidator::delete_fulfilled_context();
        mock_dependency_state_validator_delete_context
            .expect()
            .return_const(false);

        let pending_deleted_workload =
            generate_test_deleted_workload(AGENT_A.to_owned(), WORKLOAD_NAME_1.to_owned());

        let instance_name_deleted_workload = pending_deleted_workload.instance_name.clone();

        workload_scheduler.queue.insert(
            instance_name_deleted_workload.workload_name().to_owned(),
            PendingEntry::Delete(pending_deleted_workload.clone()),
        );

        workload_scheduler
            .next_workload_operations(&MockParameterStorage::default())
            .await;

        assert!(workload_state_receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn utest_next_workload_operations_keep_pending_create_in_queue() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, _workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_create_context =
            MockDependencyStateValidator::create_fulfilled_context();
        mock_dependency_state_validator_create_context
            .expect()
            .return_const(false);

        let pending_workload_spec = generate_test_workload_spec_with_param(
            AGENT_A.to_owned(),
            WORKLOAD_NAME_1.to_owned(),
            RUNTIME.to_owned(),
        );

        let instance_name_create_workload = pending_workload_spec.instance_name.clone();

        workload_scheduler.queue.insert(
            instance_name_create_workload.workload_name().to_owned(),
            PendingEntry::Create(pending_workload_spec.clone()),
        );

        let ready_workload_operations = workload_scheduler
            .next_workload_operations(&MockParameterStorage::default())
            .await;

        assert!(ready_workload_operations.is_empty());

        assert!(workload_scheduler
            .queue
            .contains_key(instance_name_create_workload.workload_name()));
    }

    #[tokio::test]
    async fn utest_next_workload_operations_no_report_pending_create_on_reenqueue() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, mut workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_create_context =
            MockDependencyStateValidator::create_fulfilled_context();
        mock_dependency_state_validator_create_context
            .expect()
            .return_const(false);

        let pending_workload_spec = generate_test_workload_spec_with_param(
            AGENT_A.to_owned(),
            WORKLOAD_NAME_1.to_owned(),
            RUNTIME.to_owned(),
        );

        let instance_name_create_workload = pending_workload_spec.instance_name.clone();

        workload_scheduler.queue.insert(
            instance_name_create_workload.workload_name().to_owned(),
            PendingEntry::Create(pending_workload_spec.clone()),
        );

        workload_scheduler
            .next_workload_operations(&MockParameterStorage::default())
            .await;

        assert!(workload_state_receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn utest_next_workload_operations_keep_pending_update_in_queue() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, _workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_create_context =
            MockDependencyStateValidator::create_fulfilled_context();
        mock_dependency_state_validator_create_context
            .expect()
            .return_const(true);

        let mock_dependency_state_validator_delete_context =
            MockDependencyStateValidator::delete_fulfilled_context();
        mock_dependency_state_validator_delete_context
            .expect()
            .return_const(false);

        let ready_workload_spec = generate_test_workload_spec_with_param(
            AGENT_A.to_owned(),
            WORKLOAD_NAME_1.to_owned(),
            RUNTIME.to_owned(),
        );

        let instance_name = ready_workload_spec.instance_name.clone();

        let pending_deleted_workload = generate_test_deleted_workload(
            instance_name.agent_name().to_owned(),
            instance_name.workload_name().to_owned(),
        );

        workload_scheduler.queue.insert(
            instance_name.workload_name().to_owned(),
            PendingEntry::UpdateDelete(
                ready_workload_spec.clone(),
                pending_deleted_workload.clone(),
            ),
        );

        let ready_workload_operations = workload_scheduler
            .next_workload_operations(&MockParameterStorage::default())
            .await;

        assert!(ready_workload_operations.is_empty());

        assert!(workload_scheduler
            .queue
            .contains_key(instance_name.workload_name()));
    }

    #[tokio::test]
    async fn utest_next_workload_operations_no_report_pending_delete_on_pending_update_reenqueue() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, mut workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_create_context =
            MockDependencyStateValidator::create_fulfilled_context();
        mock_dependency_state_validator_create_context
            .expect()
            .return_const(true);

        let mock_dependency_state_validator_delete_context =
            MockDependencyStateValidator::delete_fulfilled_context();
        mock_dependency_state_validator_delete_context
            .expect()
            .return_const(false);

        let ready_workload_spec = generate_test_workload_spec_with_param(
            AGENT_A.to_owned(),
            WORKLOAD_NAME_1.to_owned(),
            RUNTIME.to_owned(),
        );

        let instance_name = ready_workload_spec.instance_name.clone();

        let pending_deleted_workload = generate_test_deleted_workload(
            instance_name.agent_name().to_owned(),
            instance_name.workload_name().to_owned(),
        );

        workload_scheduler.queue.insert(
            instance_name.workload_name().to_owned(),
            PendingEntry::UpdateDelete(
                ready_workload_spec.clone(),
                pending_deleted_workload.clone(),
            ),
        );

        workload_scheduler
            .next_workload_operations(&MockParameterStorage::default())
            .await;

        assert!(workload_state_receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn utest_next_workload_operations_remove_ready_create_from_queue() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, _workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_create_context =
            MockDependencyStateValidator::create_fulfilled_context();
        mock_dependency_state_validator_create_context
            .expect()
            .return_const(true);

        let ready_workload_spec = generate_test_workload_spec_with_param(
            AGENT_A.to_owned(),
            WORKLOAD_NAME_1.to_owned(),
            RUNTIME.to_owned(),
        );

        workload_scheduler.queue.insert(
            ready_workload_spec.instance_name.workload_name().to_owned(),
            PendingEntry::Create(ready_workload_spec.clone()),
        );

        let ready_workload_operations = workload_scheduler
            .next_workload_operations(&MockParameterStorage::default())
            .await;

        assert_eq!(
            vec![WorkloadOperation::Create(ready_workload_spec)],
            ready_workload_operations
        );

        assert!(workload_scheduler.queue.is_empty());
    }

    #[tokio::test]
    async fn utest_next_workload_operations_remove_ready_delete_from_queue() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, _workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_delete_context =
            MockDependencyStateValidator::delete_fulfilled_context();
        mock_dependency_state_validator_delete_context
            .expect()
            .return_const(true);

        let ready_deleted_workload =
            generate_test_deleted_workload(AGENT_A.to_owned(), WORKLOAD_NAME_1.to_owned());

        workload_scheduler.queue.insert(
            ready_deleted_workload
                .instance_name
                .workload_name()
                .to_owned(),
            PendingEntry::Delete(ready_deleted_workload.clone()),
        );

        let ready_workload_operations = workload_scheduler
            .next_workload_operations(&MockParameterStorage::default())
            .await;

        assert_eq!(
            vec![WorkloadOperation::Delete(ready_deleted_workload)],
            ready_workload_operations
        );

        assert!(workload_scheduler.queue.is_empty());
    }

    #[tokio::test]
    async fn utest_next_workload_operations_remove_ready_update_create_at_most_once_from_queue() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, _workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_create_context =
            MockDependencyStateValidator::create_fulfilled_context();
        mock_dependency_state_validator_create_context
            .expect()
            .return_const(true);

        let mock_dependency_state_validator_delete_context =
            MockDependencyStateValidator::delete_fulfilled_context();
        mock_dependency_state_validator_delete_context
            .expect()
            .return_const(true);

        let ready_workload_spec = generate_test_workload_spec_with_param(
            AGENT_A.to_owned(),
            WORKLOAD_NAME_1.to_owned(),
            RUNTIME.to_owned(),
        );

        let instance_name = ready_workload_spec.instance_name.clone();

        let ready_deleted_workload = generate_test_deleted_workload(
            instance_name.agent_name().to_owned(),
            instance_name.workload_name().to_owned(),
        );

        workload_scheduler.queue.insert(
            instance_name.workload_name().to_owned(),
            PendingEntry::UpdateCreate(ready_workload_spec.clone(), ready_deleted_workload.clone()),
        );

        let ready_workload_operations = workload_scheduler
            .next_workload_operations(&MockParameterStorage::default())
            .await;

        assert_eq!(
            vec![WorkloadOperation::Update(
                ready_workload_spec,
                ready_deleted_workload
            )],
            ready_workload_operations
        );

        assert!(workload_scheduler.queue.is_empty());
    }

    #[tokio::test]
    async fn utest_next_workload_operations_remove_ready_update_delete_at_most_once_from_queue() {
        let _guard = crate::test_helper::MOCKALL_CONTEXT_SYNC
            .get_lock_async()
            .await;
        let (workload_state_sender, _workload_state_receiver) = channel(1);
        let mut workload_scheduler = WorkloadScheduler::new(workload_state_sender);

        let mock_dependency_state_validator_create_context =
            MockDependencyStateValidator::create_fulfilled_context();
        mock_dependency_state_validator_create_context
            .expect()
            .return_const(true);

        let mock_dependency_state_validator_delete_context =
            MockDependencyStateValidator::delete_fulfilled_context();
        mock_dependency_state_validator_delete_context
            .expect()
            .return_const(true);

        let ready_workload_spec = generate_test_workload_spec_with_param(
            AGENT_A.to_owned(),
            WORKLOAD_NAME_1.to_owned(),
            RUNTIME.to_owned(),
        );

        let instance_name = ready_workload_spec.instance_name.clone();

        let ready_deleted_workload = generate_test_deleted_workload(
            instance_name.agent_name().to_owned(),
            instance_name.workload_name().to_owned(),
        );

        workload_scheduler.queue.insert(
            instance_name.workload_name().to_owned(),
            PendingEntry::UpdateDelete(ready_workload_spec.clone(), ready_deleted_workload.clone()),
        );

        let ready_workload_operations = workload_scheduler
            .next_workload_operations(&MockParameterStorage::default())
            .await;

        assert_eq!(
            vec![WorkloadOperation::Update(
                ready_workload_spec,
                ready_deleted_workload
            )],
            ready_workload_operations
        );

        assert!(workload_scheduler.queue.is_empty());
    }
}
