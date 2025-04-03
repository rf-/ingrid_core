import * as ingridCore from "./pkg/ingrid_core.js";
// Removed unused import as Deno.readTextFile is used instead
// Load the word list from disk
// const wordList = await Deno.readTextFile('./resources/spreadthewordlist.dict');
// console.log("Word list loaded successfully from disk:", wordList.slice(0, 100));

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
        const result = await ingridCore.fill_grid(gridContent, null, null);
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

// New test case that loads a wordlist from disk and passes it to fill_grid
async function testGridFillWithWordList() {
    try {
        console.log("Initializing WebAssembly module...");
        await ingridCore.default();
        
        console.log("Creating grid content...");
        const gridContent = 
            ".....\n" +
            ".....\n" +
            ".....\n" +
            ".....\n" +
            ".....";
        
        console.log("Grid content:", gridContent);
        
        // Call fill_grid with the loaded word list as final argument
        const result = await ingridCore.fill_grid(gridContent, null, null, 'http://localhost:8080/spreadthewordlist.dict');
        console.log("Grid filling with word list succeeded:", result);
    } catch (error) {
        console.error("Error during grid fill with word list:", error);
        if (error instanceof Error) {
            console.error("Error name:", error.name);
            console.error("Error message:", error.message);
            console.error("Error stack:", error.stack);
        }
    }
}

// Run the tests
testGridFill();
testGridFillWithWordList();
