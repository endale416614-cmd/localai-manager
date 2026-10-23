//! One GPU app at a time: on an 8 GB card any two of them overflow VRAM (measured).

use crate::config::Group;
use crate::supervisor::Status;

/// Index of a running GPU app other than `target`, if any.
pub fn gpu_holder(apps: &[(Group, Status)], target: Option<usize>) -> Option<usize> {
    apps.iter()
        .enumerate()
        .position(|(i, (g, s))| *g == Group::Gpu && s.is_on() && Some(i) != target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Group::*;
    use Status::*;

    #[test]
    fn finds_other_running_gpu_app() {
        let apps = [(Gpu, Ready), (Gpu, Stopped), (Svc, Ready)];
        assert_eq!(gpu_holder(&apps, Some(1)), Some(0));
        assert_eq!(gpu_holder(&apps, Some(0)), None);
        assert_eq!(gpu_holder(&apps, None), Some(0));
    }

    #[test]
    fn starting_counts_stopping_does_not() {
        assert_eq!(gpu_holder(&[(Gpu, Starting), (Gpu, Stopped)], Some(1)), Some(0));
        assert_eq!(gpu_holder(&[(Gpu, Stopping), (Gpu, Stopped)], Some(1)), None);
        assert_eq!(gpu_holder(&[(Svc, Ready), (Gpu, Stopped)], Some(1)), None);
    }
}
