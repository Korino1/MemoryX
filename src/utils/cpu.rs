//! Runtime CPU capability detection.
//!
//! MemoryX release binaries are built for portability. Hot paths should use
//! this module to select optional accelerated implementations at runtime instead
//! of relying on `target-cpu=native` in the global build configuration.

/// CPU features that are useful for MemoryX storage, hashing and vector search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuFeatures {
    pub sse2: bool,
    pub sse41: bool,
    pub avx: bool,
    pub avx2: bool,
    pub avx512f: bool,
    pub avx512vl: bool,
    pub bmi1: bool,
    pub bmi2: bool,
    pub aes: bool,
    pub pclmulqdq: bool,
    pub sha: bool,
    pub neon: bool,
}

impl CpuFeatures {
    /// Detect CPU features available to the current process.
    pub fn detect() -> Self {
        let mut features = Self::default();

        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            features.sse2 = std::arch::is_x86_feature_detected!("sse2");
            features.sse41 = std::arch::is_x86_feature_detected!("sse4.1");
            features.avx = std::arch::is_x86_feature_detected!("avx");
            features.avx2 = std::arch::is_x86_feature_detected!("avx2");
            features.avx512f = std::arch::is_x86_feature_detected!("avx512f");
            features.avx512vl = std::arch::is_x86_feature_detected!("avx512vl");
            features.bmi1 = std::arch::is_x86_feature_detected!("bmi1");
            features.bmi2 = std::arch::is_x86_feature_detected!("bmi2");
            features.aes = std::arch::is_x86_feature_detected!("aes");
            features.pclmulqdq = std::arch::is_x86_feature_detected!("pclmulqdq");
            features.sha = std::arch::is_x86_feature_detected!("sha");
        }

        #[cfg(target_arch = "aarch64")]
        {
            features.neon = std::arch::is_aarch64_feature_detected!("neon");
            features.aes = std::arch::is_aarch64_feature_detected!("aes");
            features.sha = std::arch::is_aarch64_feature_detected!("sha2");
        }

        features
    }

    /// Best coarse acceleration tier for selecting specialized kernels.
    pub fn tier(self) -> CpuTier {
        if self.avx512f && self.avx512vl {
            CpuTier::X86Avx512
        } else if self.avx2 {
            CpuTier::X86Avx2
        } else if self.sse41 {
            CpuTier::X86Sse41
        } else if self.neon {
            CpuTier::Aarch64Neon
        } else {
            CpuTier::Portable
        }
    }

    /// Human-readable feature list for diagnostics and logs.
    pub fn enabled_feature_names(self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.sse2 {
            names.push("sse2");
        }
        if self.sse41 {
            names.push("sse4.1");
        }
        if self.avx {
            names.push("avx");
        }
        if self.avx2 {
            names.push("avx2");
        }
        if self.avx512f {
            names.push("avx512f");
        }
        if self.avx512vl {
            names.push("avx512vl");
        }
        if self.bmi1 {
            names.push("bmi1");
        }
        if self.bmi2 {
            names.push("bmi2");
        }
        if self.aes {
            names.push("aes");
        }
        if self.pclmulqdq {
            names.push("pclmulqdq");
        }
        if self.sha {
            names.push("sha");
        }
        if self.neon {
            names.push("neon");
        }
        names
    }
}

/// Coarse runtime CPU tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CpuTier {
    Portable,
    X86Sse41,
    X86Avx2,
    X86Avx512,
    Aarch64Neon,
}

impl CpuTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            CpuTier::Portable => "portable",
            CpuTier::X86Sse41 => "x86-sse4.1",
            CpuTier::X86Avx2 => "x86-avx2",
            CpuTier::X86Avx512 => "x86-avx512",
            CpuTier::Aarch64Neon => "aarch64-neon",
        }
    }
}

/// Return the current process CPU tier.
pub fn runtime_cpu_tier() -> CpuTier {
    CpuFeatures::detect().tier()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_is_safe_on_current_cpu() {
        let features = CpuFeatures::detect();
        let tier = features.tier();
        assert!(!tier.as_str().is_empty());
    }

    #[test]
    fn avx512_takes_priority_over_avx2() {
        let features = CpuFeatures {
            avx2: true,
            avx512f: true,
            avx512vl: true,
            ..CpuFeatures::default()
        };
        assert_eq!(features.tier(), CpuTier::X86Avx512);
    }

    #[test]
    fn avx2_takes_priority_over_sse41() {
        let features = CpuFeatures {
            sse41: true,
            avx2: true,
            ..CpuFeatures::default()
        };
        assert_eq!(features.tier(), CpuTier::X86Avx2);
    }
}
