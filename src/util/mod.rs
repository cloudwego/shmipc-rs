// Copyright 2025 CloudWeGo Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

pub mod buf_reader;
pub mod shmbuf_reader;

/// Determine whether to create new share memory based on the current remaining share memory size.
///
/// Currently, this only works on the /dev/shm partition of the Linux system. Other systems return
/// true by default (By the way, shmipc can run on Mac OS, but has not been performance tuned).
///
/// When getting /dev/shm partition information fails, returns false and outputs a log.
pub fn can_create_on_dev_shm(size: u64, path: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        if path.contains("/dev/shm") {
            match fs2::free_space("/dev/shm") {
                Ok(free) => return free >= size,
                Err(err) => {
                    tracing::warn!(
                        "could not read /dev/shm free size, can_create_on_dev_shm default return \
                         true, err: {}",
                        err
                    );
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use crate::util::can_create_on_dev_shm;

    #[test]
    fn test_can_create_on_dev_shm() {
        #[cfg(target_os = "linux")]
        {
            // just on /dev/shm, other always return true
            assert!(can_create_on_dev_shm(u64::MAX, "sdffafds"));
            let free = fs2::free_space("/dev/shm").unwrap();
            assert!(can_create_on_dev_shm(free, "/dev/shm/xxx"));
            assert!(!can_create_on_dev_shm(free + 1, "/dev/shm/yyy"));
        }
        #[cfg(target_os = "macos")]
        {
            // always return true
            assert!(can_create_on_dev_shm(33333, "sdffafds"));
        }
    }
}
