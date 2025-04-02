import * as ingridCore from '/pkg/ingrid_core';

async function testGridFill() {
    try {
        // Initialize the WebAssembly module
        await ingridCore.default();
        
        // Test input grid
        const gridContent = "HELLO_WORLD"; // Replace with actual grid format
        
        // Call the fill_grid function with optional parameters
        const result = ingridCore.fill_grid(gridContent, 0.8, 3);
        
        console.log('Grid filling result:', result);
    } catch (error) {
        console.error('Error testing grid fill:', error);
    }
}

// Run the test
testGridFill();

