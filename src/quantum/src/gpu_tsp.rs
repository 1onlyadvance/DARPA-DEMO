//! GPU-Accelerated TSP Solver using CUDA
//!
//! Implements high-performance Traveling Salesman Problem solving with:
//! - GPU-parallel distance matrix computation
//! - GPU-parallel 2-opt swap evaluation (O(n²) swaps per iteration)
//! - Greedy nearest-neighbor construction (CPU)
//! - Iterative 2-opt improvement (GPU)

use ndarray::Array2;
use num_complex::Complex64;
use anyhow::{Result, Context, anyhow};
use cudarc::driver::{CudaDevice, LaunchAsync, LaunchConfig};
use std::sync::Arc;
use std::collections::HashSet;

/// GPU-accelerated TSP solver
pub struct GpuTspSolver {
    /// CUDA device
    device: Arc<CudaDevice>,
    /// Number of cities
    n_cities: usize,
    /// Distance matrix (CPU copy)
    distance_matrix: Array2<f64>,
    /// Current best tour
    tour: Vec<usize>,
    /// Current tour length
    tour_length: f64,
}

impl GpuTspSolver {
    /// Create new GPU TSP solver from coupling matrix
    ///
    /// # Arguments
    /// * `coupling_matrix` - Complex coupling amplitudes between cities
    pub fn new(coupling_matrix: &Array2<Complex64>) -> Result<Self> {
        let n = coupling_matrix.nrows();

        if n < 2 {
            return Err(anyhow!("TSP requires at least 2 cities, got {}", n));
        }

        if coupling_matrix.ncols() != n {
            return Err(anyhow!("Coupling matrix must be square, got {}×{}", n, coupling_matrix.ncols()));
        }

        // Initialize CUDA device
        let device = CudaDevice::new(0)
            .context("Failed to initialize CUDA device 0. Check:\n  \
                     1. NVIDIA driver is installed (nvidia-smi)\n  \
                     2. GPU is accessible from WSL2 (/dev/dxg exists)\n  \
                     3. LD_LIBRARY_PATH includes /usr/lib/wsl/lib")?;

        println!("🔧 Initializing GPU TSP Solver for {} cities...", n);

        // Compute distance matrix on GPU
        let distance_matrix = Self::compute_distance_matrix_gpu(&device, coupling_matrix)?;

        // Initialize with nearest-neighbor tour (CPU)
        let tour = Self::nearest_neighbor_tour(&distance_matrix);
        let tour_length = Self::calculate_tour_length(&tour, &distance_matrix);

        println!("  ✓ Initial tour (nearest-neighbor): length = {:.2}", tour_length);

        Ok(Self {
            device,
            n_cities: n,
            distance_matrix,
            tour,
            tour_length,
        })
    }

    /// Compute distance matrix on GPU
    fn compute_distance_matrix_gpu(
        device: &Arc<CudaDevice>,
        coupling_matrix: &Array2<Complex64>,
    ) -> Result<Array2<f64>> {
        let n = coupling_matrix.nrows();

        // Load PTX kernels
        let out_dir = std::env::var("OUT_DIR")
            .map_err(|_| anyhow!("OUT_DIR not set - build script may have failed"))?;
        let ptx_path = std::path::Path::new(&out_dir).join("tsp_solver.ptx");

        if !ptx_path.exists() {
            return Err(anyhow!(
                "PTX file not found at {:?}. Build script may have failed. Run: cargo clean && cargo build",
                ptx_path
            ));
        }

        let ptx = std::fs::read_to_string(&ptx_path)
            .map_err(|e| anyhow!("Failed to load PTX from {:?}: {}", ptx_path, e))?;

        if ptx.is_empty() || !ptx.contains("compute_distance_matrix") {
            return Err(anyhow!(
                "Invalid PTX file: missing compute_distance_matrix kernel. Rebuild with: cargo clean && cargo build"
            ));
        }

        device.load_ptx(ptx.into(), "tsp_solver", &[
            "compute_distance_matrix",
            "find_max_distance",
            "normalize_distances",
        ]).context("Failed to load PTX kernels - check CUDA driver version")?;

        let compute_fn = device.get_func("tsp_solver", "compute_distance_matrix")
            .context("Failed to get compute_distance_matrix kernel")?;
        let find_max_fn = device.get_func("tsp_solver", "find_max_distance")
            .context("Failed to get find_max_distance kernel")?;
        let normalize_fn = device.get_func("tsp_solver", "normalize_distances")
            .context("Failed to get normalize_distances kernel")?;

        // Prepare coupling matrix data (split real/imag)
        let mut coupling_real = vec![0.0f32; n * n];
        let mut coupling_imag = vec![0.0f32; n * n];

        for i in 0..n {
            for j in 0..n {
                let c = coupling_matrix[[i, j]];
                coupling_real[i * n + j] = c.re as f32;
                coupling_imag[i * n + j] = c.im as f32;
            }
        }

        // Upload to GPU
        let gpu_coupling_real = device.htod_sync_copy(&coupling_real)?;
        let gpu_coupling_imag = device.htod_sync_copy(&coupling_imag)?;
        let gpu_distances = device.alloc_zeros::<f32>(n * n)?;

        // Launch distance computation kernel
        let threads = 256;
        let blocks = (n * n + threads - 1) / threads;
        let cfg = LaunchConfig {
            grid_dim: (blocks as u32, 1, 1),
            block_dim: (threads as u32, 1, 1),
            shared_mem_bytes: 0,
        };

        unsafe {
            compute_fn.launch(cfg, (
                &gpu_coupling_real,
                &gpu_coupling_imag,
                &gpu_distances,
                n as u32,
            ))?;
        }

        device.synchronize()?;

        // Find maximum distance for normalization
        let num_blocks = (n * n + threads - 1) / threads;
        let gpu_partial_maxs = device.alloc_zeros::<f32>(num_blocks)?;

        let cfg_max = LaunchConfig {
            grid_dim: (num_blocks as u32, 1, 1),
            block_dim: (threads as u32, 1, 1),
            shared_mem_bytes: (threads * std::mem::size_of::<f32>()) as u32,
        };

        unsafe {
            find_max_fn.launch(cfg_max, (
                &gpu_distances,
                &gpu_partial_maxs,
                n as u32,
            ))?;
        }

        device.synchronize()?;

        // Download partial maxs and find global max
        let partial_maxs: Vec<f32> = device.dtoh_sync_copy(&gpu_partial_maxs)?;
        let max_distance = partial_maxs.iter().cloned().fold(0.0f32, f32::max);

        // Normalize distances
        unsafe {
            normalize_fn.launch(cfg, (
                &gpu_distances,
                max_distance,
                n as u32,
            ))?;
        }

        device.synchronize()?;

        // Download distance matrix
        let distances_flat: Vec<f32> = device.dtoh_sync_copy(&gpu_distances)?;

        // Convert to Array2<f64>
        let mut distance_matrix = Array2::zeros((n, n));
        for i in 0..n {
            for j in 0..n {
                distance_matrix[[i, j]] = distances_flat[i * n + j] as f64;
            }
        }

        Ok(distance_matrix)
    }

    /// Nearest neighbor heuristic for initial tour (CPU)
    fn nearest_neighbor_tour(distance_matrix: &Array2<f64>) -> Vec<usize> {
        let n = distance_matrix.nrows();
        if n == 0 {
            return Vec::new();
        }

        let mut tour = Vec::with_capacity(n);
        let mut unvisited: HashSet<usize> = (0..n).collect();

        // Start from city 0
        let mut current = 0;
        tour.push(current);
        unvisited.remove(&current);

        // Greedily visit nearest unvisited city
        while !unvisited.is_empty() {
            let mut nearest = *unvisited.iter().next().unwrap();
            let mut min_dist = distance_matrix[[current, nearest]];

            for &v in &unvisited {
                let dist = distance_matrix[[current, v]];
                if dist < min_dist {
                    min_dist = dist;
                    nearest = v;
                }
            }

            tour.push(nearest);
            unvisited.remove(&nearest);
            current = nearest;
        }

        tour
    }

    /// Calculate total tour length (CPU)
    fn calculate_tour_length(tour: &[usize], distance_matrix: &Array2<f64>) -> f64 {
        if tour.len() < 2 {
            return 0.0;
        }

        let mut length = 0.0;
        for i in 0..tour.len() {
            let from = tour[i];
            let to = tour[(i + 1) % tour.len()];
            length += distance_matrix[[from, to]];
        }

        length
    }

    /// Optimize tour using GPU-accelerated 2-opt
    pub fn optimize_2opt_gpu(&mut self, max_iterations: usize) -> Result<()> {
        // Load PTX kernels
        let out_dir = std::env::var("OUT_DIR")
            .map_err(|_| anyhow!("OUT_DIR not set - build script may have failed"))?;
        let ptx_path = std::path::Path::new(&out_dir).join("tsp_solver.ptx");

        if !ptx_path.exists() {
            return Err(anyhow!(
                "PTX file not found at {:?}. Build script may have failed. Run: cargo clean && cargo build",
                ptx_path
            ));
        }

        let ptx = std::fs::read_to_string(&ptx_path)
            .map_err(|e| anyhow!("Failed to load PTX from {:?}: {}", ptx_path, e))?;

        if ptx.is_empty() || !ptx.contains("evaluate_2opt_swaps") {
            return Err(anyhow!(
                "Invalid PTX file: missing evaluate_2opt_swaps kernel. Rebuild with: cargo clean && cargo build"
            ));
        }

        self.device.load_ptx(ptx.into(), "tsp_solver", &[
            "evaluate_2opt_swaps",
            "find_min_delta",
            "apply_2opt_swap",
        ]).context("Failed to load PTX kernels - check CUDA driver version")?;

        println!("🔄 Running GPU 2-opt optimization (max {} iterations)...", max_iterations);

        let n = self.n_cities;
        let total_swaps = n * (n - 3) / 2;

        if total_swaps == 0 {
            return Ok(()); // No valid swaps for small tours
        }

        // Upload distance matrix to GPU
        let mut distances_flat = vec![0.0f32; n * n];
        for i in 0..n {
            for j in 0..n {
                distances_flat[i * n + j] = self.distance_matrix[[i, j]] as f32;
            }
        }
        let gpu_distances = self.device.htod_sync_copy(&distances_flat)?;

        let mut improved = true;
        let mut iteration = 0;
        let mut improvements = 0;

        while improved && iteration < max_iterations {
            improved = false;

            // Get kernel functions
            let evaluate_fn = self.device.get_func("tsp_solver", "evaluate_2opt_swaps")
                .context("Failed to get evaluate_2opt_swaps kernel")?;
            let find_min_fn = self.device.get_func("tsp_solver", "find_min_delta")
                .context("Failed to get find_min_delta kernel")?;

            // Upload current tour to GPU
            let tour_u32: Vec<u32> = self.tour.iter().map(|&v| v as u32).collect();
            let gpu_tour = self.device.htod_sync_copy(&tour_u32)?;

            // Allocate GPU memory for results
            let gpu_deltas = self.device.alloc_zeros::<f32>(total_swaps)?;
            let gpu_swap_pairs = self.device.alloc_zeros::<u32>(total_swaps * 2)?;

            // Launch evaluation kernel
            let threads = 256;
            let blocks = (total_swaps + threads - 1) / threads;
            let cfg = LaunchConfig {
                grid_dim: (blocks as u32, 1, 1),
                block_dim: (threads as u32, 1, 1),
                shared_mem_bytes: 0,
            };

            unsafe {
                evaluate_fn.launch(cfg, (
                    &gpu_distances,
                    &gpu_tour,
                    &gpu_deltas,
                    &gpu_swap_pairs,
                    n as u32,
                ))?;
            }

            self.device.synchronize()?;

            // Find minimum delta (best improvement)
            let num_blocks = (total_swaps + threads - 1) / threads;
            let gpu_partial_mins = self.device.alloc_zeros::<f32>(num_blocks)?;
            let gpu_partial_indices = self.device.alloc_zeros::<u32>(num_blocks)?;

            let cfg_min = LaunchConfig {
                grid_dim: (num_blocks as u32, 1, 1),
                block_dim: (threads as u32, 1, 1),
                shared_mem_bytes: (threads * (std::mem::size_of::<f32>() + std::mem::size_of::<u32>())) as u32,
            };

            unsafe {
                find_min_fn.launch(cfg_min, (
                    &gpu_deltas,
                    &gpu_partial_mins,
                    &gpu_partial_indices,
                    total_swaps as u32,
                ))?;
            }

            self.device.synchronize()?;

            // Download partial results and find global minimum
            let partial_mins: Vec<f32> = self.device.dtoh_sync_copy(&gpu_partial_mins)?;
            let partial_indices: Vec<u32> = self.device.dtoh_sync_copy(&gpu_partial_indices)?;

            let mut best_delta = partial_mins[0];
            let mut best_idx = partial_indices[0] as usize;

            for i in 1..partial_mins.len() {
                if partial_mins[i] < best_delta {
                    best_delta = partial_mins[i];
                    best_idx = partial_indices[i] as usize;
                }
            }

            // If improvement found, apply swap (on CPU for simplicity)
            if best_delta < -1e-6 {
                let swap_pairs: Vec<u32> = self.device.dtoh_sync_copy(&gpu_swap_pairs)?;
                let i = swap_pairs[best_idx * 2] as usize;
                let j = swap_pairs[best_idx * 2 + 1] as usize;

                // Apply 2-opt swap (reverse segment between i+1 and j)
                let mut k = i + 1;
                let mut l = j;
                while k < l {
                    self.tour.swap(k, l);
                    k += 1;
                    l -= 1;
                }

                self.tour_length += best_delta as f64;
                improved = true;
                improvements += 1;

                if iteration % 10 == 0 {
                    println!("  Iteration {}: tour length = {:.2} (improved by {:.4})",
                             iteration, self.tour_length, -best_delta);
                }
            }

            iteration += 1;
        }

        println!("  ✓ Optimization complete: {} iterations, {} improvements", iteration, improvements);
        println!("  ✓ Final tour length: {:.2}", self.tour_length);

        Ok(())
    }

    /// Get current tour
    pub fn get_tour(&self) -> &[usize] {
        &self.tour
    }

    /// Get tour length
    pub fn get_tour_length(&self) -> f64 {
        self.tour_length
    }

    /// Validate tour (all cities visited exactly once)
    pub fn validate_tour(&self) -> bool {
        if self.tour.len() != self.n_cities {
            return false;
        }

        let unique: HashSet<_> = self.tour.iter().collect();
        unique.len() == self.n_cities
    }

    /// Calculate tour quality compared to a known optimal
    pub fn calculate_quality(&self, optimal_length: f64) -> f64 {
        if optimal_length < 1e-10 {
            return 1.0;
        }
        optimal_length / self.tour_length
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gpu_tsp_creation() {
        let coupling = Array2::from_elem((5, 5), Complex64::new(0.5, 0.0));
        let tsp = GpuTspSolver::new(&coupling);

        assert!(tsp.is_ok());
        let tsp = tsp.unwrap();
        assert_eq!(tsp.n_cities, 5);
        assert_eq!(tsp.tour.len(), 5);
        assert!(tsp.validate_tour());
    }

    #[test]
    fn test_gpu_2opt_optimization() {
        // Create asymmetric coupling (should find better tour)
        let mut coupling = Array2::zeros((6, 6));
        for i in 0..6 {
            for j in 0..6 {
                let dist = (i as f64 - j as f64).abs();
                coupling[[i, j]] = Complex64::new(1.0 / (1.0 + dist), 0.0);
            }
        }

        let mut tsp = GpuTspSolver::new(&coupling).unwrap();
        let initial_length = tsp.get_tour_length();

        tsp.optimize_2opt_gpu(100).unwrap();

        assert!(tsp.get_tour_length() <= initial_length);
        assert!(tsp.validate_tour());
    }
}
