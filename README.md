# Voxara Studio (`voxara.exe`)

Aplikasi desktop **100% Native Rust** (`eframe` + `cpal` WASAPI + `hound`) tanpa ketergantungan Python, PowerShell, internet, ataupun API berbayar. Dirancang sebagai **Megafon Artikulasi Real-Time** yang membuat suara asli Anda terdengar jauh lebih jernih dan stabil pada kalimat panjang saat *online meeting* (Zoom / Google Meet / Teams), rekaman suara, dan *voiceover* konten.

## Fitur Utama (100% Suara Asli Anda)
1. **Megafon Live Real-Time (Lock-Free & Zero-Allocation)**:
   - **Penstabil Napas Kalimat Panjang (Syllabic AGC)**: Menjaga volume kata di akhir kalimat panjang agar tidak melemah akibat kebocoran napas.
   - **Peredam Sengau Adaptif (210–380 Hz & 1 kHz)**: Mengurangi resonansi dengung hidung secara dinamis.
   - **Kejernihan Vokal Mulut ($F_1$ 650 Hz & $F_2$ 1900 Hz) + Penguat Konsonan (P/T/K/S) + De-Esser**: Memperjelas transien konsonan tanpa desis menusuk.
   - **Latensi**: Latensi diukur dari timestamp WASAPI bila driver menyediakannya; jika tidak, angka yang tampil hanyalah perkiraan.
2. **Soundboard Suara Asli Saya (Meeting 1-Klik)**:
   - Rekam kalimat rapat menggunakan suara asli Anda (otomatis dibersihkan DSP dan disimpan ke `data/slot_N.wav`), lalu putar 1-klik ke Virtual Mic dengan *auto-ducking*.
3. **Studio Rekaman & Teleprompter Voiceover Konten**:
   - Rekam suara, lihat metrik spektrum ternormalisasi (dB), atau tulis naskah multi-scene di **Teleprompter**, rekam per scene dengan suara asli Anda, lalu gabungkan seluruh scene menjadi master `.wav` ber-*timestamp* di folder `output/`.
4. **Auto-Kalibrasi Akustik (5 Detik)**:
   - Memindai frekuensi `215–375 Hz` dari sampel suara Anda untuk menemukan puncak resonansi sengau pribadi dan menyetel filter secara otomatis.

## Cara Pakai untuk Meeting (Zoom / Google Meet / Microsoft Teams)
1. Jalankan `voxara.exe` (atau klik dua kali `Voxara.bat`).
2. Agar lawan bicara di Zoom/Meet mendengar suara Anda yang sudah diproses, pasang driver virtual gratis **VB-Audio Virtual Cable**, lalu pilih **`CABLE Input`** sebagai **Output / Virtual Mic** di Voxara, dan pilih **`CABLE Output`** sebagai Mikrofon di Zoom/Meet.
   - Voxara otomatis memilih **`CABLE Input`** sebagai Output / Virtual Mic bila ditemukan dan belum ada pilihan output yang tersimpan.
   - *(Opsional)* Ganti nama mic virtual agar mudah dikenali: klik **Ganti Nama Mic jadi "Voxara Mic"** di Voxara (membuka panel Sound Windows di tab Recording), atau buka sendiri tab Recording:
     1. Di tab Recording, klik kanan **"CABLE Output"** → Properties.
     2. Ganti nama menjadi **"Voxara Mic"** → OK.
     3. Di Zoom/Meet/Teams pilih **"Voxara Mic"** sebagai mikrofon.
   - Windows mungkin tetap menampilkan "(VB-Audio Virtual Cable)" di belakang nama — itu dari driver dan normal.
   - Jangan ganti nama "CABLE Input" kecuali Anda juga memilihnya ulang di Voxara.
3. Aktifkan **Aktifkan Mesin Audio**, lalu nyalakan **Nyalakan Megafon Live** *(gunakan headphone atau Virtual Mic agar tidak terjadi feedback dari speaker)*.

## Kompilasi & Pengujian
```powershell
cargo test
cargo build --release
```
