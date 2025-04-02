import * as ingridCore from "./pkg/ingrid_core.js";

async function testGridFill() {
    try {
        console.log("Initializing WebAssembly module...");
        await ingridCore.default();
        
        console.log("Creating grid content...");
        // Test input grid (5x5)
        const gridContent = 
            ".....\\n" +
            ".....\\n" +
            ".....\\n" +
            ".....\\n" +
            ".....";
        
        console.log("Grid content:", gridContent);
        console.log("Attempting to fill grid with parameters:", {
            min_score: 0,
            max_shared_substring: 3
        });
        
        const result = ingridCore.fill_grid(gridContent, 0, 3);
        console.log("Grid filling succeeded:", result);
    } catch (error) {
        console.error("Error during grid fill:", error);
        // Log more details about the error
        if (error instanceof Error) {
            console.error("Error name:", error.name);
            console.error("Error message:", error.message);
            console.error("Error stack:", error.stack);
        }
    }
}

// Run the test
testGridFill();
