// Import the WebAssembly module
import init, { fill_grid } from '../ingrid_core.js';

// Initialize the WebAssembly module
async function initializeWasm() {
    try {
        await init();
        console.log('WebAssembly module initialized successfully');
        document.getElementById('loading').style.display = 'none';
    } catch (error) {
        console.error('Error initializing WebAssembly module:', error);
        displayError('Failed to initialize WebAssembly module: ' + error.message);
    }
}

// Display error message
function displayError(message) {
    const resultElement = document.getElementById('result');
    resultElement.innerHTML = `<span class="error">Error: ${message}</span>`;
}

// Display the grid result
function displayResult(result) {
    const resultElement = document.getElementById('result');
    resultElement.textContent = result;
}

// Handle form submission
async function handleSubmit(event) {
    event.preventDefault();
    
    const gridTemplate = document.getElementById('grid-template').value.trim();
    const minScoreInput = document.getElementById('min-score').value;
    const maxSharedSubstringInput = document.getElementById('max-shared-substring').value;
    
    // Convert empty strings to null for optional parameters
    const minScore = minScoreInput ? parseInt(minScoreInput, 10) : null;
    const maxSharedSubstring = maxSharedSubstringInput ? parseInt(maxSharedSubstringInput, 10) : null;
    
    document.getElementById('loading').style.display = 'block';
    
    try {
        const result = fill_grid(gridTemplate, minScore, maxSharedSubstring);
        displayResult(result);
    } catch (error) {
        console.error('Error filling grid:', error);
        displayError(error.toString());
    } finally {
        document.getElementById('loading').style.display = 'none';
    }
}

// Initialize the app
document.addEventListener('DOMContentLoaded', () => {
    initializeWasm();
    document.getElementById('grid-form').addEventListener('submit', handleSubmit);
});

