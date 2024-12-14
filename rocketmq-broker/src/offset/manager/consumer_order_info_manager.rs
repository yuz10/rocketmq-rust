/*
 * Licensed to the Apache Software Foundation (ASF) under one or more
 * contributor license agreements.  See the NOTICE file distributed with
 * this work for additional information regarding copyright ownership.
 * The ASF licenses this file to You under the Apache License, Version 2.0
 * (the "License"); you may not use this file except in compliance with
 * the License.  You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::Deref;
use std::sync::Arc;

use cheetah_string::CheetahString;
use rocketmq_common::common::broker::broker_config::BrokerConfig;
use rocketmq_common::common::config_manager::ConfigManager;
use rocketmq_common::utils::serde_json_utils::SerdeJsonUtils;
use rocketmq_common::TimeUtils::get_current_millis;
use serde::Deserialize;
use serde::Serialize;
use tracing::warn;

use crate::broker_path_config_helper::get_consumer_order_info_path;
use crate::offset::manager::consumer_order_info_lock_manager::ConsumerOrderInfoLockManager;

const TOPIC_GROUP_SEPARATOR: &str = "@";
const CLEAN_SPAN_FROM_LAST: u64 = 24 * 3600 * 1000;

#[derive(Default)]
pub(crate) struct ConsumerOrderInfoManager {
    pub(crate) broker_config: Arc<BrokerConfig>,
    pub(crate) consumer_order_info_wrapper: parking_lot::Mutex<ConsumerOrderInfoWrapper>,
    pub(crate) consumer_order_info_lock_manager: Option<ConsumerOrderInfoLockManager>,
}

//Fully implemented will be removed
#[allow(unused_variables)]
impl ConfigManager for ConsumerOrderInfoManager {
    fn config_file_path(&self) -> String {
        get_consumer_order_info_path(self.broker_config.store_path_root_dir.as_str())
    }

    fn encode_pretty(&self, pretty_format: bool) -> String {
        self.auto_clean();
        let wrapper = self.consumer_order_info_wrapper.lock();
        match pretty_format {
            true => SerdeJsonUtils::to_json_pretty(&wrapper.table)
                .expect("Failed to serialize consumer order info wrapper"),
            false => serde_json::to_string(&wrapper.table)
                .expect("Failed to serialize consumer order info wrapper"),
        }
    }

    fn decode(&self, json_string: &str) {
        if json_string.is_empty() {
            return;
        }
        let wrapper =
            serde_json::from_str::<ConsumerOrderInfoWrapper>(json_string).unwrap_or_default();
        if !wrapper.table.is_empty() {
            self.consumer_order_info_wrapper
                .lock()
                .table
                .clone_from(&wrapper.table);
            if self.consumer_order_info_lock_manager.is_some() {
                self.consumer_order_info_lock_manager
                    .as_ref()
                    .unwrap()
                    .recover(self.consumer_order_info_wrapper.lock().deref());
            }
        }
    }
}

impl ConsumerOrderInfoManager {
    fn auto_clean(&self) {}

    pub fn update_next_visible_time(
        &self,
        topic: &CheetahString,
        group: &CheetahString,
        queue_id: i32,
        queue_offset: u64,
        pop_time: u64,
        next_visible_time: u64,
    ) {
        let key = build_key(topic, group);
        let mut table = self.consumer_order_info_wrapper.lock();
        let qs = table.table.get_mut(&key);
        if qs.is_none() {
            warn!(
                "orderInfo of queueId is null. key: {}, queueOffset: {}, queueId: {}",
                key, queue_offset, queue_id
            );
            return;
        }
        let qs = qs.unwrap();
        let order_info = qs.get_mut(&queue_id);
        if order_info.is_none() {
            warn!(
                "orderInfo of queueId is null. key: {}, queueOffset: {}, queueId: {}",
                key, queue_offset, queue_id
            );
            return;
        }
        let order_info = order_info.unwrap();
        if pop_time != order_info.pop_time {
            warn!(
                "popTime is not equal to orderInfo saved. key: {}, queueOffset: {}, orderInfo: \
                 {}, popTime: {}",
                key, queue_offset, queue_id, pop_time,
            );
            return;
        }
        order_info.update_offset_next_visible_time(queue_offset, next_visible_time);
        self.update_lock_free_timestamp(topic, group, queue_id, order_info);
    }

    fn update_lock_free_timestamp(
        &self,
        _topic: &CheetahString,
        _group: &CheetahString,
        _queue_id: i32,
        _order_info: &OrderInfo,
    ) {
        unimplemented!("")
    }
}

#[inline]
#[must_use]
fn build_key(topic: &CheetahString, group: &CheetahString) -> String {
    format!("{}{}{}", topic, TOPIC_GROUP_SEPARATOR, group)
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub(crate) struct ConsumerOrderInfoWrapper {
    table: HashMap<String /* topic@group */, HashMap<i32, OrderInfo>>,
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub(crate) struct OrderInfo {
    #[serde(rename = "popTime")]
    pop_time: u64,
    #[serde(rename = "i")]
    invisible_time: Option<u64>,
    #[serde(rename = "0")]
    offset_list: Vec<u64>,
    #[serde(rename = "ot")]
    offset_next_visible_time: HashMap<u64, u64>,
    #[serde(rename = "oc")]
    offset_consumed_count: HashMap<u64, i32>,
    #[serde(rename = "l")]
    last_consume_timestamp: u64,
    #[serde(rename = "cm")]
    commit_offset_bit: u64,
    #[serde(rename = "a")]
    attempt_id: String,
}

impl OrderInfo {
    /// Builds a list of offsets from a given list of queue offsets.
    /// If the list contains only one element, it returns the same list.
    /// Otherwise, it returns a list where each element is the difference
    /// between the current and the first element.
    ///
    /// # Arguments
    ///
    /// * `queue_offset_list` - A vector of queue offsets.
    ///
    /// # Returns
    ///
    /// A vector of offsets.
    pub fn build_offset_list(queue_offset_list: Vec<u64>) -> Vec<u64> {
        let mut simple = Vec::new();
        if queue_offset_list.len() == 1 {
            simple.extend(queue_offset_list);
            return simple;
        }
        let first = queue_offset_list[0];
        simple.push(first);
        for item in queue_offset_list.iter().skip(1) {
            simple.push(*item - first);
        }
        simple
    }

    /// Determines if the current order info needs to be blocked based on the attempt ID
    /// and the current invisible time.
    ///
    /// # Arguments
    ///
    /// * `attempt_id` - The attempt ID to check.
    /// * `current_invisible_time` - The current invisible time.
    ///
    /// # Returns
    ///
    /// `true` if the order info needs to be blocked, `false` otherwise.
    pub fn need_block(&mut self, attempt_id: &str, current_invisible_time: u64) -> bool {
        if self.offset_list.is_empty() {
            return false;
        }
        if self.attempt_id == attempt_id {
            return false;
        }
        let num = self.offset_list.len();
        if self.invisible_time.is_none() || self.invisible_time.unwrap_or(0) == 0 {
            self.invisible_time = Some(current_invisible_time);
        }
        let current_time = get_current_millis();
        for (i, _) in (0..num).enumerate() {
            if self.is_not_ack(i) {
                let mut next_visible_time = self.pop_time + self.invisible_time.unwrap_or(0);
                if let Some(time) = self.offset_next_visible_time.get(&self.get_queue_offset(i)) {
                    next_visible_time = *time;
                }
                if current_time < next_visible_time {
                    return true;
                }
            }
        }
        false
    }

    /// Gets the lock-free timestamp for the current order info.
    ///
    /// # Returns
    ///
    /// An `Option<u64>` containing the lock-free timestamp if available, `None` otherwise.
    pub fn get_lock_free_timestamp(&self) -> Option<u64> {
        if self.offset_list.is_empty() {
            return None;
        }
        let current_time = get_current_millis();
        for i in 0..self.offset_list.len() {
            if self.is_not_ack(i) {
                if self.invisible_time.is_none() || self.invisible_time.unwrap_or(0) == 0 {
                    return None;
                }
                let mut next_visible_time = self.pop_time + self.invisible_time.unwrap_or(0);
                if let Some(time) = self.offset_next_visible_time.get(&self.get_queue_offset(i)) {
                    next_visible_time = *time;
                }
                if current_time < next_visible_time {
                    return Some(next_visible_time);
                }
            }
        }
        Some(current_time)
    }

    /// Updates the next visible time for a given queue offset.
    ///
    /// # Arguments
    ///
    /// * `queue_offset` - The queue offset to update.
    /// * `next_visible_time` - The next visible time to set.
    #[inline]
    pub fn update_offset_next_visible_time(&mut self, queue_offset: u64, next_visible_time: u64) {
        self.offset_next_visible_time
            .insert(queue_offset, next_visible_time);
    }

    /// Gets the next offset for the current order info.
    ///
    /// # Returns
    ///
    /// An `i64` representing the next offset. Returns -2 if the offset list is empty.
    pub fn get_next_offset(&self) -> i64 {
        if self.offset_list.is_empty() {
            return -2;
        }
        let mut i = 0;
        for j in 0..self.offset_list.len() {
            if self.is_not_ack(j) {
                break;
            }
            i += 1;
        }
        if i == self.offset_list.len() {
            self.get_queue_offset(self.offset_list.len() - 1) as i64 + 1
        } else {
            self.get_queue_offset(i) as i64
        }
    }

    /// Gets the queue offset for a given offset index.
    ///
    /// # Arguments
    ///
    /// * `offset_index` - The index of the offset to get.
    ///
    /// # Returns
    ///
    /// A `u64` representing the queue offset.
    pub fn get_queue_offset(&self, offset_index: usize) -> u64 {
        if offset_index == 0 {
            return self.offset_list[0];
        }
        self.offset_list[0] + self.offset_list[offset_index]
    }

    /// Checks if the offset at the given index is not acknowledged.
    ///
    /// # Arguments
    ///
    /// * `offset_index` - The index of the offset to check.
    ///
    /// # Returns
    ///
    /// `true` if the offset is not acknowledged, `false` otherwise.
    pub fn is_not_ack(&self, offset_index: usize) -> bool {
        if offset_index >= 64 {
            return false;
        }
        (self.commit_offset_bit & (1 << offset_index)) == 0
    }

    /// Merges the offset consumed count with the previous attempt ID and offset list.
    ///
    /// # Arguments
    ///
    /// * `pre_attempt_id` - The previous attempt ID.
    /// * `pre_offset_list` - The previous offset list.
    /// * `prev_offset_consumed_count` - The previous offset consumed count.
    pub fn merge_offset_consumed_count(
        &mut self,
        pre_attempt_id: &str,
        pre_offset_list: Vec<u64>,
        prev_offset_consumed_count: HashMap<u64, i32>,
    ) {
        let mut offset_consumed_count = HashMap::new();
        if pre_attempt_id == self.attempt_id {
            self.offset_consumed_count = prev_offset_consumed_count;
            return;
        }
        let mut pre_queue_offset_set = HashSet::new();
        for (index, _) in pre_offset_list.iter().enumerate() {
            pre_queue_offset_set.insert(Self::get_queue_offset_from_list(&pre_offset_list, index));
        }
        for i in 0..self.offset_list.len() {
            let queue_offset = self.get_queue_offset(i);
            if pre_queue_offset_set.contains(&queue_offset) {
                let mut count = 1;
                if let Some(pre_count) = prev_offset_consumed_count.get(&queue_offset) {
                    count += pre_count;
                }
                offset_consumed_count.insert(queue_offset, count);
            }
        }
        self.offset_consumed_count = offset_consumed_count;
    }

    /// Gets the queue offset from a list of offsets for a given index.
    ///
    /// # Arguments
    ///
    /// * `pre_offset_list` - The list of previous offsets.
    /// * `offset_index` - The index of the offset to get.
    ///
    /// # Returns
    ///
    /// A `u64` representing the queue offset.
    fn get_queue_offset_from_list(pre_offset_list: &[u64], offset_index: usize) -> u64 {
        if offset_index == 0 {
            return pre_offset_list[0];
        }
        pre_offset_list[0] + pre_offset_list[offset_index]
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn build_offset_list_with_single_element() {
        let queue_offset_list = vec![10];
        let expected = vec![10];
        assert_eq!(OrderInfo::build_offset_list(queue_offset_list), expected);
    }

    #[test]
    fn need_block_returns_false_for_empty_offset_list() {
        let mut order_info = OrderInfo {
            pop_time: 1000,
            invisible_time: Some(3000),
            offset_list: vec![],
            offset_next_visible_time: HashMap::new(),
            offset_consumed_count: HashMap::new(),
            last_consume_timestamp: 0,
            commit_offset_bit: 0,
            attempt_id: "test".to_string(),
        };
        assert!(!order_info.need_block("another_test", 2000));
    }

    #[test]
    fn get_lock_free_timestamp_returns_none_for_empty_offset_list() {
        let order_info = OrderInfo {
            pop_time: 1000,
            invisible_time: Some(3000),
            offset_list: vec![],
            offset_next_visible_time: HashMap::new(),
            offset_consumed_count: HashMap::new(),
            last_consume_timestamp: 0,
            commit_offset_bit: 0,
            attempt_id: "test".to_string(),
        };
        assert_eq!(order_info.get_lock_free_timestamp(), None);
    }

    #[test]
    fn get_next_offset_returns_minus_two_for_empty_offset_list() {
        let order_info = OrderInfo {
            pop_time: 1000,
            invisible_time: Some(3000),
            offset_list: vec![],
            offset_next_visible_time: HashMap::new(),
            offset_consumed_count: HashMap::new(),
            last_consume_timestamp: 0,
            commit_offset_bit: 0,
            attempt_id: "test".to_string(),
        };
        assert_eq!(order_info.get_next_offset(), -2);
    }

    #[test]
    fn merge_offset_consumed_count_with_same_attempt_id() {
        let mut order_info = OrderInfo {
            pop_time: 0,
            invisible_time: None,
            offset_list: vec![1, 2, 3],
            offset_next_visible_time: HashMap::new(),
            offset_consumed_count: HashMap::new(),
            last_consume_timestamp: 0,
            commit_offset_bit: 0,
            attempt_id: "test".to_string(),
        };
        let pre_offset_list = vec![1, 2];
        let prev_offset_consumed_count = HashMap::from([(1, 1), (2, 1)]);
        order_info.merge_offset_consumed_count("test", pre_offset_list, prev_offset_consumed_count);
        assert_eq!(order_info.offset_consumed_count.get(&1), Some(&1));
        assert_eq!(order_info.offset_consumed_count.get(&2), Some(&1));
    }
}
