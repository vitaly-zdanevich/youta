//! Best-effort RAM cache headroom below 90% physical and container memory use.
//!
//! The returned allowance is dynamic headroom, not 90% of the machine assigned
//! to this application's cache. Existing cached bytes are already part of the
//! measured usage. Callers must measure again before growth and periodically
//! evict under pressure. Concurrent allocations, measurement races, decoder
//! overhead, and allocator retention mean this is not a hard process RSS cap.
//!
//! Linux counters follow the kernel documentation for
//! [`MemAvailable`](https://docs.kernel.org/filesystems/proc.html#meminfo) and
//! [cgroup memory limits](https://docs.kernel.org/admin-guide/cgroup-v2.html#memory).

#[cfg(target_os = "linux")]
use std::path::{Component, Path, PathBuf};

/// Returns the packet-cache allowance below the tightest observable 90% limit.
///
/// Includes the current cache in the resulting allowance, or subtracts memory
/// pressure from it when the system has passed the threshold. `None` means the
/// measurements are unavailable or malformed: callers must decline retention,
/// not assume the machine has spare RAM. Non-Linux platforms currently decline
/// retention until equivalent physical/container measurements are implemented.
pub(super) fn cache_allowance(current_cache_bytes: u64) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        MemorySnapshot::read()?.cache_allowance(current_cache_bytes)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = current_cache_bytes;
        None
    }
}

/// One physical or hierarchical container limit, in bytes.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MemoryLimit {
    total: u64,
    used: u64,
}

#[cfg(target_os = "linux")]
impl MemoryLimit {
    /// Preserves a rounded-up 10% reserve without multiplying large counters.
    fn cache_allowance(self, current_cache_bytes: u64) -> u64 {
        if self.total == 0 {
            return 0;
        }
        let reserve = self.total / 10 + u64::from(self.total % 10 != 0);
        let ceiling = self.total - reserve;
        if self.used <= ceiling {
            current_cache_bytes.saturating_add(ceiling - self.used)
        } else {
            current_cache_bytes.saturating_sub(self.used - ceiling)
        }
    }
}

/// Physical memory plus finite, observable cgroup limits, including ancestors.
#[cfg(target_os = "linux")]
struct MemorySnapshot {
    limits: Vec<MemoryLimit>,
}

#[cfg(target_os = "linux")]
impl MemorySnapshot {
    /// Uses the most restrictive domain instead of adding overlapping budgets.
    fn cache_allowance(&self, current_cache_bytes: u64) -> Option<u64> {
        self.limits
            .iter()
            .map(|limit| limit.cache_allowance(current_cache_bytes))
            .min()
    }

    /// Reads bounded procfs records and the calling process's memory hierarchies.
    #[cfg(target_os = "linux")]
    fn read() -> Option<Self> {
        let mut snapshot = Self {
            limits: vec![physical_memory(
                &read_text(Path::new("/proc/meminfo"), 64 * 1024).ok()?,
            )?],
        };
        let groups = memberships(&read_text(Path::new("/proc/self/cgroup"), 16 * 1024).ok()?)?;
        if groups.is_empty() {
            return Some(snapshot);
        }
        let mountinfo = read_text(Path::new("/proc/self/mountinfo"), 1024 * 1024).ok()?;
        for group in groups {
            let (root, directory) = mount_for(&group, &mountinfo)?;
            if !directory.is_dir() {
                return None;
            }
            snapshot.add_cgroup_limits(&root, &directory, group.version, &mut |path| {
                match read_text(path, 128) {
                    Ok(value) => Ok(Some(value)),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(error),
                }
            })?;
        }
        Some(snapshot)
    }

    /// Walks no further than the visible mount root; namespace-hidden ancestors
    /// cannot be measured, so this remains a best-effort pressure signal.
    fn add_cgroup_limits(
        &mut self,
        root: &Path,
        directory: &Path,
        version: CgroupVersion,
        read: &mut impl FnMut(&Path) -> std::io::Result<Option<String>>,
    ) -> Option<()> {
        if !directory.starts_with(root) {
            return None;
        }
        let (limit_name, usage_name) = match version {
            CgroupVersion::V1 => ("memory.limit_in_bytes", "memory.usage_in_bytes"),
            CgroupVersion::V2 => ("memory.max", "memory.current"),
        };
        let mut directory = directory;
        // Bound both filesystem work and the number of retained measurements.
        for _ in 0..64 {
            let limit = read(&directory.join(limit_name)).ok()?;
            if let Some(limit) = limit {
                if let Some(total) = cgroup_limit(&limit, version)? {
                    let usage = read(&directory.join(usage_name)).ok()??;
                    self.limits.push(MemoryLimit {
                        total,
                        used: byte_count(&usage)?,
                    });
                }
            } else {
                // v2 root has no memory.max. A disabled memory controller has
                // neither file; do not mistake other partial records for it.
                let usage = read(&directory.join(usage_name)).ok()?;
                if usage.is_some() && !(version == CgroupVersion::V2 && directory == root) {
                    return None;
                }
                if version == CgroupVersion::V1 {
                    return None;
                }
            }
            if directory == root {
                return Some(());
            }
            directory = directory.parent()?;
        }
        None
    }
}

/// Reads a small kernel pseudo-file without trusting its often-zero metadata size.
#[cfg(target_os = "linux")]
fn read_text(path: &Path, maximum: u64) -> std::io::Result<String> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(path)?
        .take(maximum + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > maximum {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "memory measurement exceeds its bound",
        ));
    }
    Ok(text)
}

/// Parses physical available memory, including the kernel's reclaimable estimate.
#[cfg(target_os = "linux")]
fn physical_memory(text: &str) -> Option<MemoryLimit> {
    let mut total = None;
    let mut available = None;
    for line in text.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let target = match name {
            "MemTotal" => &mut total,
            "MemAvailable" => &mut available,
            _ => continue,
        };
        let mut fields = value.split_whitespace();
        let bytes = byte_count(fields.next()?)?.checked_mul(1024)?;
        if fields.next() != Some("kB") || fields.next().is_some() || target.replace(bytes).is_some()
        {
            return None;
        }
    }
    let total = total.filter(|total| *total > 0)?;
    let used = total.checked_sub(available?)?;
    Some(MemoryLimit { total, used })
}

/// Requires an unsigned decimal kernel byte counter, with outer whitespace only.
#[cfg(target_os = "linux")]
fn byte_count(text: &str) -> Option<u64> {
    let text = text.trim();
    (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CgroupVersion {
    V1,
    V2,
}

/// Distinguishes a valid unlimited record from malformed or overflowing input.
#[cfg(target_os = "linux")]
fn cgroup_limit(text: &str, version: CgroupVersion) -> Option<Option<u64>> {
    if version == CgroupVersion::V2 && text.trim() == "max" {
        Some(None)
    } else {
        byte_count(text).map(Some)
    }
}

#[cfg(target_os = "linux")]
struct CgroupMembership {
    version: CgroupVersion,
    path: PathBuf,
}

/// Extracts only memory-bearing membership records, rejecting ambiguous duplicates.
#[cfg(target_os = "linux")]
fn memberships(text: &str) -> Option<Vec<CgroupMembership>> {
    let mut groups: Vec<CgroupMembership> = Vec::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let mut fields = line.splitn(3, ':');
        byte_count(fields.next()?)?;
        let controllers = fields.next()?;
        let path = fields.next()?;
        let version = if controllers.is_empty() {
            CgroupVersion::V2
        } else if controllers
            .split(',')
            .any(|controller| controller == "memory")
        {
            CgroupVersion::V1
        } else {
            continue;
        };
        if groups.iter().any(|group| group.version == version) || !safe_absolute(Path::new(path)) {
            return None;
        }
        groups.push(CgroupMembership {
            version,
            path: path.into(),
        });
    }
    Some(groups)
}

/// Locates the most specific visible mount and maps its delegated root safely.
#[cfg(target_os = "linux")]
fn mount_for(group: &CgroupMembership, text: &str) -> Option<(PathBuf, PathBuf)> {
    let mut best = None;
    let mut best_depth = 0;
    for line in text.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let mut right = right.split_whitespace();
        let kind = right.next()?;
        let _source = right.next()?;
        let super_options = right.next()?;
        let matches = match group.version {
            CgroupVersion::V2 => kind == "cgroup2",
            CgroupVersion::V1 => {
                kind == "cgroup" && super_options.split(',').any(|option| option == "memory")
            }
        };
        if !matches {
            continue;
        }
        let mut fields = left.split_whitespace();
        let mount_root = mount_path(fields.nth(3)?)?;
        let mount_point = mount_path(fields.next()?)?;
        let Ok(relative) = group.path.strip_prefix(&mount_root) else {
            continue;
        };
        let depth = mount_root.components().count();
        if depth > best_depth {
            best_depth = depth;
            best = Some((mount_point.clone(), mount_point.join(relative)));
        }
    }
    best
}

/// Decodes the kernel's mountinfo path escapes, never shell or percent escapes.
#[cfg(target_os = "linux")]
fn mount_path(text: &str) -> Option<PathBuf> {
    let mut result = Vec::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            result.push(bytes[index]);
            index += 1;
            continue;
        }
        let escaped = match bytes.get(index + 1..index + 4)? {
            b"040" => b' ',
            b"011" => b'\t',
            b"012" => b'\n',
            b"134" => b'\\',
            _ => return None,
        };
        result.push(escaped);
        index += 4;
    }
    let path = PathBuf::from(String::from_utf8(result).ok()?);
    safe_absolute(&path).then_some(path)
}

/// Refuses relative paths and parent traversal when mapping virtual filesystems.
#[cfg(target_os = "linux")]
fn safe_absolute(path: &Path) -> bool {
    path.is_absolute()
        && !path.as_os_str().as_encoded_bytes().contains(&0)
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// Reclaimable page cache counts as available RAM; MemFree is not the policy.
    #[test]
    fn physical_memory_uses_available_instead_of_free() {
        let memory = physical_memory(
            "MemTotal: 1000 kB\nMemFree: 1 kB\nMemAvailable: 400 kB\nCached: 399 kB\n",
        )
        .unwrap();
        assert_eq!(
            memory,
            MemoryLimit {
                total: 1_024_000,
                used: 614_400
            }
        );
        assert_eq!(memory.cache_allowance(1024), 308_224);
    }

    /// Cache growth consumes only current headroom, never 90% of total RAM itself.
    #[test]
    fn headroom_and_pressure_adjust_the_existing_cache() {
        assert_eq!(
            MemoryLimit {
                total: 1000,
                used: 800
            }
            .cache_allowance(40),
            140
        );
        assert_eq!(
            MemoryLimit {
                total: 1000,
                used: 900
            }
            .cache_allowance(40),
            40
        );
        assert_eq!(
            MemoryLimit {
                total: 1000,
                used: 925
            }
            .cache_allowance(40),
            15
        );
        assert_eq!(
            MemoryLimit {
                total: 1000,
                used: 990
            }
            .cache_allowance(40),
            0
        );
        assert_eq!(
            MemoryLimit {
                total: 1000,
                used: 1200
            }
            .cache_allowance(40),
            0
        );
    }

    /// Reserve the final fractional byte instead of rounding the 10% reserve down.
    #[test]
    fn reserves_ten_percent_rounded_up_without_overflow() {
        assert_eq!(MemoryLimit { total: 11, used: 0 }.cache_allowance(0), 9);
        assert_eq!(MemoryLimit { total: 1, used: 0 }.cache_allowance(0), 0);
        assert_eq!(MemoryLimit { total: 0, used: 0 }.cache_allowance(10), 0);
        assert_eq!(
            MemoryLimit {
                total: u64::MAX,
                used: 0
            }
            .cache_allowance(0),
            16_602_069_666_338_596_453
        );
        assert_eq!(
            MemoryLimit {
                total: u64::MAX,
                used: 0
            }
            .cache_allowance(u64::MAX),
            u64::MAX
        );
    }

    /// Malformed measurements decline caching rather than silently guessing capacity.
    #[test]
    fn rejects_missing_malformed_and_overflowing_physical_memory() {
        for input in [
            "MemTotal: 1 kB\n",
            "MemTotal: 0 kB\nMemAvailable: 0 kB\n",
            "MemTotal: 1 kB\nMemAvailable: 2 kB\n",
            "MemTotal: 1 MB\nMemAvailable: 0 kB\n",
            "MemTotal: +1 kB\nMemAvailable: 0 kB\n",
            "MemTotal: 18446744073709551615 kB\nMemAvailable: 0 kB\n",
            "MemTotal: 1 kB\nMemAvailable: 0 kB\nMemTotal: 2 kB\n",
            "MemTotal: 1 kB\nMemAvailable: 0 kB extra\n",
        ] {
            assert!(physical_memory(input).is_none(), "accepted {input:?}");
        }
    }

    /// A parent cgroup shared with other processes can be tighter than the leaf.
    #[test]
    fn tightest_physical_or_container_limit_wins() {
        let snapshot = MemorySnapshot {
            limits: vec![
                MemoryLimit {
                    total: 10_000,
                    used: 1000,
                },
                MemoryLimit {
                    total: 1000,
                    used: 820,
                },
                MemoryLimit {
                    total: 2000,
                    used: 1825,
                },
            ],
        };
        assert_eq!(snapshot.cache_allowance(40), Some(15));
        assert_eq!(MemorySnapshot { limits: vec![] }.cache_allowance(40), None);
    }

    /// The v2 unlimited marker is not a byte count; v1 limits remain numeric.
    #[test]
    fn cgroup_numbers_and_unlimited_marker_are_strict() {
        assert_eq!(cgroup_limit("max\n", CgroupVersion::V2), Some(None));
        assert_eq!(cgroup_limit("512\n", CgroupVersion::V2), Some(Some(512)));
        assert_eq!(cgroup_limit("0", CgroupVersion::V1), Some(Some(0)));
        for value in ["", "-1", "+1", "1 kB", "18446744073709551616"] {
            assert_eq!(cgroup_limit(value, CgroupVersion::V2), None);
        }
        assert_eq!(cgroup_limit("max", CgroupVersion::V1), None);
    }

    /// Only the unified and legacy memory hierarchies constrain the RAM budget.
    #[test]
    fn selects_v2_and_v1_memory_memberships() {
        let groups =
            memberships("0::/user.slice/session\n5:cpu,cpuacct:/cpu\n4:memory,blkio:/legacy\n")
                .unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].version, CgroupVersion::V2);
        assert_eq!(groups[0].path, Path::new("/user.slice/session"));
        assert_eq!(groups[1].version, CgroupVersion::V1);
        assert_eq!(groups[1].path, Path::new("/legacy"));
        assert!(memberships("0::/../../outside\n").is_none());
        assert!(memberships("0::relative\n").is_none());
        assert!(memberships("0::/a\n0::/b\n").is_none());
    }

    /// A delegated mount's root must be removed before resolving its filesystem path.
    #[test]
    fn maps_mount_roots_and_escaped_mount_paths() {
        let group = CgroupMembership {
            version: CgroupVersion::V2,
            path: "/tenant/app".into(),
        };
        let info = "22 1 0:23 /tenant /sys/fs/cgroup\\040private rw - cgroup2 cgroup rw\n";
        assert_eq!(
            mount_for(&group, info),
            Some((
                PathBuf::from("/sys/fs/cgroup private"),
                PathBuf::from("/sys/fs/cgroup private/app")
            ))
        );
        assert!(
            mount_for(
                &group,
                "22 1 0:23 /other /sys/fs/cgroup rw - cgroup2 cgroup rw\n"
            )
            .is_none()
        );
        let group = CgroupMembership {
            version: CgroupVersion::V1,
            path: "/app".into(),
        };
        assert_eq!(
            mount_for(
                &group,
                "22 1 0:23 / /sys/fs/cgroup/memory rw - cgroup cgroup rw,memory\n"
            ),
            Some((
                PathBuf::from("/sys/fs/cgroup/memory"),
                PathBuf::from("/sys/fs/cgroup/memory/app")
            ))
        );
    }

    /// Limits in shared parents must count even when the leaf is unlimited.
    #[test]
    fn measures_ancestors_and_handles_the_unlimited_v2_root() {
        let mut snapshot = MemorySnapshot {
            limits: vec![MemoryLimit {
                total: 10_000,
                used: 1000,
            }],
        };
        let mut reads = Vec::new();
        snapshot
            .add_cgroup_limits(
                Path::new("/sys/cgroup"),
                Path::new("/sys/cgroup/team/leaf"),
                CgroupVersion::V2,
                &mut |path| {
                    reads.push(path.to_owned());
                    Ok(match path.to_str().unwrap() {
                        "/sys/cgroup/team/leaf/memory.max" => Some("max\n".into()),
                        "/sys/cgroup/team/memory.max" => Some("1000\n".into()),
                        "/sys/cgroup/team/memory.current" => Some("950\n".into()),
                        "/sys/cgroup/memory.current" => Some("1000\n".into()),
                        _ => None,
                    })
                },
            )
            .unwrap();
        assert_eq!(snapshot.cache_allowance(200), Some(150));
        assert_eq!(
            reads.last().unwrap(),
            Path::new("/sys/cgroup/memory.current")
        );
        assert!(reads.iter().all(|path| path.starts_with("/sys/cgroup")));
    }

    /// Legacy cgroups use their own limit and usage counters, including at root.
    #[test]
    fn measures_v1_limits_without_interpreting_unlimited_sentinel_as_small() {
        let mut snapshot = MemorySnapshot {
            limits: vec![MemoryLimit {
                total: 1000,
                used: 800,
            }],
        };
        snapshot
            .add_cgroup_limits(
                Path::new("/memory"),
                Path::new("/memory/app"),
                CgroupVersion::V1,
                &mut |path| {
                    Ok(Some(
                        match path.to_str().unwrap() {
                            "/memory/app/memory.limit_in_bytes" => "500",
                            "/memory/app/memory.usage_in_bytes" => "430",
                            "/memory/memory.limit_in_bytes" => "9223372036854771712",
                            "/memory/memory.usage_in_bytes" => "1000",
                            other => panic!("unexpected read {other}"),
                        }
                        .into(),
                    ))
                },
            )
            .unwrap();
        assert_eq!(snapshot.cache_allowance(10), Some(30));
    }

    /// Partial, unreadable, or malformed cgroup measurements must not lift limits.
    #[test]
    fn unavailable_container_measurements_decline_caching() {
        for (limit, usage) in [
            (Some("500"), None),
            (Some("bad"), Some("1")),
            (Some("500"), Some("-1")),
            (None, Some("1")),
        ] {
            let mut snapshot = MemorySnapshot { limits: vec![] };
            assert!(
                snapshot
                    .add_cgroup_limits(
                        Path::new("/memory"),
                        Path::new("/memory/app"),
                        CgroupVersion::V2,
                        &mut |path| Ok(if path.ends_with("memory.max") {
                            limit
                        } else {
                            usage
                        }
                        .map(str::to_owned)),
                    )
                    .is_none()
            );
        }
        let mut snapshot = MemorySnapshot { limits: vec![] };
        assert!(
            snapshot
                .add_cgroup_limits(
                    Path::new("/memory"),
                    Path::new("/memory/app"),
                    CgroupVersion::V2,
                    &mut |_| Err(std::io::ErrorKind::PermissionDenied.into()),
                )
                .is_none()
        );
    }

    /// Path namespace escapes and excessive hierarchy depth cannot expand work.
    #[test]
    fn refuses_outside_mount_and_excessive_hierarchy_depth() {
        let mut snapshot = MemorySnapshot { limits: vec![] };
        assert!(
            snapshot
                .add_cgroup_limits(
                    Path::new("/memory"),
                    Path::new("/other/app"),
                    CgroupVersion::V2,
                    &mut |_| panic!("outside mount must not be read"),
                )
                .is_none()
        );
        let deep = (0..65).fold(PathBuf::from("/memory"), |path, _| path.join("child"));
        let mut reads = 0;
        assert!(
            snapshot
                .add_cgroup_limits(Path::new("/memory"), &deep, CgroupVersion::V2, &mut |_| {
                    reads += 1;
                    Ok(Some("max".into()))
                },)
                .is_none()
        );
        assert_eq!(reads, 64);
    }

    /// Kernel records have fixed small bounds even when their file size is zero.
    #[test]
    fn bounded_reader_rejects_oversized_measurements() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("counter");
        std::fs::write(&path, "12345").unwrap();
        assert_eq!(read_text(&path, 5).unwrap(), "12345");
        assert_eq!(
            read_text(&path, 4).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }
}
