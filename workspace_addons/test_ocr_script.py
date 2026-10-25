import sys
from PIL import Image, ImageDraw
import pytesseract
from pathlib import Path

WORKSPACE = Path("/home/mrsilhouette/orion-workspace")

def create_test_image(text="ORION OS V44 OCR TEST"):
    img_path = WORKSPACE / "test_ocr.png"
    # Larger image with clean high-contrast sans-serif style
    img = Image.new('RGB', (600, 150), color = (255, 255, 255))
    d = ImageDraw.Draw(img)
    d.text((30, 50), text, fill=(0, 0, 0))
    img.save(img_path)
    return img_path

def run_ocr(image_path):
    try:
        # psm 7 treats the image as a single text line
        text = pytesseract.image_to_string(Image.open(image_path), config='--psm 7')
        return text.strip()
    except Exception as e:
        return f"OCR Error: {e}"

if __name__ == "__main__":
    print("Creating cleaner test image...")
    img_file = create_test_image()
    
    print("Running OCR verification...")
    result = run_ocr(img_file)
    print(f"OCR Extracted Text: '{result}'")
    if "ORION" in result:
        print("Verification SUCCESS: OCR extracted text accurately!")
    else:
        print("Verification FAILED: Text did not match expected output.")
