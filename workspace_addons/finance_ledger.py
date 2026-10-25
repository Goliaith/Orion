import sqlite3
import pandas as pd
from datetime import datetime
from pathlib import Path

WORKSPACE = Path("/home/mrsilhouette/orion-workspace")
DB_PATH = WORKSPACE / "business_finance.db"

class BusinessLedger:
    def __init__(self):
        self.db_path = DB_PATH
        self._init_db()

    def _init_db(self):
        with sqlite3.connect(self.db_path) as conn:
            conn.execute("""
                CREATE TABLE IF NOT EXISTS invoices (
                    invoice_id TEXT PRIMARY KEY,
                    client_name TEXT,
                    amount REAL,
                    status TEXT,
                    date TEXT
                )
            """)
            conn.execute("""
                CREATE TABLE IF NOT EXISTS expenses (
                    expense_id INTEGER PRIMARY KEY AUTOINCREMENT,
                    category TEXT,
                    amount REAL,
                    description TEXT,
                    date TEXT
                )
            """)
            conn.commit()

    def add_invoice(self, invoice_id: str, client_name: str, amount: float, status: str = "Unpaid"):
        date = datetime.now().strftime("%Y-%m-%d")
        with sqlite3.connect(self.db_path) as conn:
            conn.execute(
                "INSERT OR REPLACE INTO invoices (invoice_id, client_name, amount, status, date) VALUES (?, ?, ?, ?, ?)",
                (invoice_id, client_name, amount, status, date)
            )
            conn.commit()
        print(f"Invoice {invoice_id} for {client_name} (${amount:.2f}) added.")

    def add_expense(self, category: str, amount: float, description: str):
        date = datetime.now().strftime("%Y-%m-%d")
        with sqlite3.connect(self.db_path) as conn:
            conn.execute(
                "INSERT INTO expenses (category, amount, description, date) VALUES (?, ?, ?, ?)",
                (category, amount, description, date)
            )
            conn.commit()
        print(f"Expense logged: [{category}] ${amount:.2f} - {description}")

    def generate_report(self):
        with sqlite3.connect(self.db_path) as conn:
            invoices_df = pd.read_sql("SELECT * FROM invoices", conn)
            expenses_df = pd.read_sql("SELECT * FROM expenses", conn)

        total_invoiced = invoices_df['amount'].sum() if not invoices_df.empty else 0.0
        total_paid = invoices_df[invoices_df['status'] == 'Paid']['amount'].sum() if not invoices_df.empty else 0.0
        total_expenses = expenses_df['amount'].sum() if not expenses_df.empty else 0.0
        net_profit = total_paid - total_expenses

        report = f"""=== Orion Business Financial Report ===
Generated: {datetime.now().strftime('%Y-%m-%d %H:%M:%S')}

[Invoices Summary]
- Total Invoiced: ${total_invoiced:.2f}
- Total Paid:     ${total_paid:.2f}
- Total Unpaid:   ${(total_invoiced - total_paid):.2f}

[Expenses Summary]
- Total Expenses: ${total_expenses:.2f}

[Net Financial Position]
- Net Profit:     ${net_profit:.2f}
=========================================
"""
        print(report)
        return report

if __name__ == "__main__":
    ledger = BusinessLedger()
    ledger.add_invoice("INV-1001", "Acme Corp", 1500.00, "Paid")
    ledger.add_invoice("INV-1002", "Globex Inc", 2800.00, "Unpaid")
    ledger.add_expense("Software & Cloud", 120.00, "Local Server Hosting")
    ledger.add_expense("Hardware", 450.00, "Peripherals")
    ledger.generate_report()
