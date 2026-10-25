import sqlite3
import hashlib
from pathlib import Path

class PersistentEmbeddingCache:
    def __init__(self, db_path="/home/mrsilhouette/.local/share/orion/embedding_cache.db"):
        self.db_path = Path(db_path)
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._init_db()

    def _init_db(self):
        with sqlite3.connect(self.db_path) as conn:
            conn.execute("""
                CREATE TABLE IF NOT EXISTS embeddings (
                    file_path TEXT,
                    file_hash TEXT,
                    embedding BLOB,
                    PRIMARY KEY (file_path, file_hash)
                )
            """)
            conn.commit()

    def get_cached(self, file_path: str, file_hash: str):
        with sqlite3.connect(self.db_path) as conn:
            cursor = conn.execute(
                "SELECT embedding FROM embeddings WHERE file_path = ? AND file_hash = ?",
                (file_path, file_hash)
            )
            row = cursor.fetchone()
            if row:
                return row[0]
        return None

    def set_cached(self, file_path: str, file_hash: str, embedding_bytes: bytes):
        with sqlite3.connect(self.db_path) as conn:
            conn.execute(
                "INSERT OR REPLACE INTO embeddings (file_path, file_hash, embedding) VALUES (?, ?, ?)",
                (file_path, file_hash, embedding_bytes)
            )
            conn.commit()

if __name__ == "__main__":
    cache = PersistentEmbeddingCache()
    print("Persistent Embedding Cache initialized successfully.")
