import sqlite3
import numpy as np
from pathlib import Path

WORKSPACE = Path("/home/mrsilhouette/orion-workspace")
DB_PATH = Path("/home/mrsilhouette/.local/share/orion/embedding_cache.db")

def search_workspace(query_embedding, limit=3):
    # Simulated semantic search against cached workspace embeddings
    if not DB_PATH.exists():
        print("No embedding cache found.")
        return []
    
    with sqlite3.connect(DB_PATH) as conn:
        cursor = conn.execute("SELECT file_path, file_hash FROM embeddings LIMIT ?", (limit,))
        results = cursor.fetchall()
    return results

if __name__ == "__main__":
    print("Semantic Search CLI initialized.")
    matches = search_workspace(None)
    print(f"Indexed files in cache: {len(matches)}")
    for m in matches:
        print(f" - {m[0]} (hash: {m[1][:8]})")
