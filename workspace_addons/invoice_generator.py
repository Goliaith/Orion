import sys
import sqlite3
import pandas as pd
from datetime import datetime
from pathlib import Path
from reportlab.lib.pagesizes import letter
from reportlab.pdfgen import canvas

WORKSPACE = Path("/home/mrsilhouette/orion-workspace")
DB_PATH = WORKSPACE / "business_finance.db"

def generate_invoice_pdf(invoice_id, output_pdf_path=None):
    if not output_pdf_path:
        output_pdf_path = WORKSPACE / f"invoice_{invoice_id}.pdf"
    
    with sqlite3.connect(DB_PATH) as conn:
        df = pd.read_sql("SELECT * FROM invoices WHERE invoice_id = ?", conn, params=(invoice_id,))
    
    if df.empty:
        print(f"Invoice {invoice_id} not found in database.")
        return False
    
    inv = df.iloc[0]
    
    c = canvas.Canvas(str(output_pdf_path), pagesize=letter)
    width, height = letter
    
    # Header
    c.setFont("Helvetica-Bold", 22)
    c.drawString(50, height - 50, "INVOICE")
    
    c.setFont("Helvetica", 12)
    c.drawString(50, height - 80, f"Invoice ID: {inv['invoice_id']}")
    c.drawString(50, height - 100, f"Date: {inv['date']}")
    c.drawString(50, height - 120, f"Status: {inv['status']}")
    
    # Client Info
    c.setFont("Helvetica-Bold", 14)
    c.drawString(50, height - 160, "Billed To:")
    c.setFont("Helvetica", 12)
    c.drawString(50, height - 180, f"{inv['client_name']}")
    
    # Table Header
    c.setFillColorRGB(0.9, 0.9, 0.9)
    c.rect(50, height - 240, width - 100, 25, fill=1, stroke=0)
    c.setFillColorRGB(0, 0, 0)
    c.setFont("Helvetica-Bold", 11)
    c.drawString(60, height - 232, "Description")
    c.drawString(450, height - 232, "Amount")
    
    # Line Item
    c.setFont("Helvetica", 11)
    c.drawString(60, height - 270, f"Professional Services / Deliverables ({inv['invoice_id']})")
    c.drawRightString(width - 60, height - 270, f"${inv['amount']:.2f}")
    
    # Total Line
    c.line(50, height - 290, width - 50, height - 290)
    c.setFont("Helvetica-Bold", 14)
    c.drawString(350, height - 320, "Total Due:")
    c.drawRightString(width - 60, height - 320, f"${inv['amount']:.2f}")
    
    # Footer
    c.setFont("Helvetica-Oblique", 10)
    c.drawString(50, 50, "Thank you for your business! Generated locally by Orion OS.")
    
    c.save()
    print(f"Professional PDF invoice generated: {output_pdf_path}")
    return output_pdf_path

if __name__ == "__main__":
    if len(sys.argv) > 1:
        generate_invoice_pdf(sys.argv[1])
    else:
        generate_invoice_pdf("INV-1001")
