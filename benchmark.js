// benchmark.js - Performance testing for ingrid_core's fill_grid function
import init, { fill_grid } from './pkg/ingrid_core.js';

// Configuration
const ITERATIONS = 100;  // Number of times to run the benchmark
const WARMUP_ITERATIONS = 5;  // Number of iterations to run before recording results
const TEST_GRID = `....
....
....
....`;  // Our test grid
// URL to fetch the word list
const wordListURL = 'http://localhost:8000/resources/spreadthewordlist.dict';


export async function runBenchmark() {
  console.log('Initializing WebAssembly module...');
  await init();
  console.log('WebAssembly module initialized');

  const results = {
    total: [],
    stringConversion: [],
    gridInitialization: [],
    solving: []
  };

  // Warm-up phase
  console.log(`Running ${WARMUP_ITERATIONS} warm-up iterations...`);
  for (let i = 0; i < WARMUP_ITERATIONS; i++) {
    await runIteration(null); // Discard results
  }

  // Benchmark phase
  console.log(`Running ${ITERATIONS} benchmark iterations...`);
  for (let i = 0; i < ITERATIONS; i++) {
    const iterationResults = await runIteration(results);
    if (i % 10 === 0) {
      console.log(`Completed ${i} iterations...`);
    }
  }

  // Calculate and return results
  return displayResults(results);
}

async function runIteration(results) {
  if (!results) {
    // Just run without collecting results for warm-up
    fill_grid(TEST_GRID, null, null, wordListURL);
    return;
  }

  // Measure total time
  const totalStart = performance.now();
  
  // Measure string conversion overhead
  const stringConversionStart = performance.now();
  const gridStr = TEST_GRID;
  const stringConversionEnd = performance.now();
  
  // Assume grid initialization is part of the initial processing in fill_grid
  // This is a simulated split since we don't have direct access to internal timings
  // Real implementation would use the web_sys::console performance markers from Rust
  const gridInitStart = performance.now();
  const solutionPromise = fill_grid(gridStr, null, null, wordListURL);
  const gridInitEnd = performance.now();
  
  // Wait for the solution (solving phase)
  const solvingStart = performance.now();
  const solution = await solutionPromise;
  const solvingEnd = performance.now();
  
  const totalEnd = performance.now();
  
  // Record results
  results.total.push(totalEnd - totalStart);
  results.stringConversion.push(stringConversionEnd - stringConversionStart);
  results.gridInitialization.push(gridInitEnd - gridInitStart);
  results.solving.push(solvingEnd - solvingStart);
  
  return {
    total: totalEnd - totalStart,
    stringConversion: stringConversionEnd - stringConversionStart,
    gridInitialization: gridInitEnd - gridInitStart,
    solving: solvingEnd - solvingStart,
    solution
  };
}

function displayResults(results) {
  let output = '\n===== BENCHMARK RESULTS =====';
  
  // Helper function to calculate statistics
  const calculateStats = (array) => {
    const sum = array.reduce((a, b) => a + b, 0);
    const avg = sum / array.length;
    const sorted = [...array].sort((a, b) => a - b);
    const median = sorted[Math.floor(sorted.length / 2)];
    const min = sorted[0];
    const max = sorted[sorted.length - 1];
    return { avg, median, min, max };
  };

  // Display each metric
  for (const [metric, values] of Object.entries(results)) {
    const stats = calculateStats(values);
    output += `\n\n--- ${metric.toUpperCase()} ---`;
    output += `\nAverage: ${stats.avg.toFixed(3)} ms`;
    output += `\nMedian: ${stats.median.toFixed(3)} ms`;
    output += `\nMin: ${stats.min.toFixed(3)} ms`;
    output += `\nMax: ${stats.max.toFixed(3)} ms`;
  }

  // Calculate and display percentages
  const totalAvg = calculateStats(results.total).avg;
  output += '\n\n--- PERCENTAGES ---';
  for (const [metric, values] of Object.entries(results)) {
    if (metric !== 'total') {
      const metricAvg = calculateStats(values).avg;
      const percentage = (metricAvg / totalAvg) * 100;
      output += `\n${metric}: ${percentage.toFixed(2)}% of total time`;
    }
  }

  output += '\n\n=============================';
  
  return output;
}
