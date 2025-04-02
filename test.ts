import * as ingridCore from "./pkg/ingrid_core.js";

async function testGridFill() {
    try {
        console.log("Initializing WebAssembly module...");
        await ingridCore.default();
        
        console.log("Creating grid content...");
        // Test input grid (5x5)
        const gridContent = 
            ".....\n" +
            ".....\n" +
            ".....\n" +
            ".....\n" +
            ".....";
        
        console.log("Grid content:", gridContent);
        
        // Call fill_grid with only the grid template, letting other parameters be null
        const result = ingridCore.fill_grid(gridContent, null, null);
        console.log("Grid filling succeeded:", result);
    } catch (error) {
        console.error("Error during grid fill:", error);
        if (error instanceof Error) {
            console.error("Error name:", error.name);
            console.error("Error message:", error.message);
            console.error("Error stack:", error.stack);
        }
    }
}

// Run the test
testGridFill();
