/* Эталонный рендер NSF в WAV — обёртка вокруг libgme (Game Music Emu).

   Для NSF не нашлось готовой консольной программы, а без эталона весь
   тракт NES проверялся только на «не молчит». libgme (blargg) играет
   NSF, а заодно GBS, KSS, HES, AY и SAP — то есть закрывает и часть
   бэклога, когда до него дойдут руки.

   Сборка (libgme в репозиторий не вносится, как и libvgm с gbsplay):
     git clone --depth 1 https://github.com/libgme/game-music-emu.git gme
     cmake -S gme -B gme/build -DCMAKE_BUILD_TYPE=Release && cmake --build gme/build -j
     g++ -O2 -I gme -I gme/demo -o gme2wav scripts/gme2wav.c gme/demo/Wave_Writer.cpp \
         -L gme/build/gme -lgme -Wl,-rpath,$PWD/gme/build/gme

   Запуск: gme2wav файл подпесня секунды выход.wav [маска_глушения]
   Маска — биты голосов, которые надо заглушить (для NSF: 1 меандр1,
   2 меандр2, 4 треугольник, 8 шум, 16 DMC). Нужна, чтобы понять,
   какой голос у нас потерян: глушим его у эталона и смотрим, стал
   ли эталон похож на наш выход.
   Подпесни считаются с единицы — как в заголовке GBS и у gbsplay. На
   разной нумерации я уже обжёгся: сравнение шло по разным мелодиям и
   выглядело как дефект звука.
*/
#include "gme/gme.h"
#include "Wave_Writer.h"
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char** argv) {
    if (argc < 5) { fprintf(stderr, "gme2wav файл подпесня секунды выход.wav [маска]\n"); return 2; }
    int mute = argc >= 6 ? (int)strtol(argv[5], 0, 0) : 0;
    long rate = 44100;
    int track = atoi(argv[2]) - 1; if (track < 0) track = 0;
    double secs = atof(argv[3]);
    Music_Emu* emu;
    gme_err_t err = gme_open_file(argv[1], &emu, rate);
    if (err) { fprintf(stderr, "gme: %s\n", err); return 1; }
    if (mute) gme_mute_voices(emu, mute);
    err = gme_start_track(emu, track);
    if (err) { fprintf(stderr, "gme: %s\n", err); return 1; }
    wave_open(rate, argv[4]);
    wave_enable_stereo();
    long total = (long)(secs * rate);
    short buf[4096];
    while (total > 0) {
        long n = total > 2048 ? 2048 : total;
        if ((err = gme_play(emu, (int)(n * 2), buf))) break;
        wave_write(buf, n * 2);
        total -= n;
    }
    wave_close();
    gme_delete(emu);
    return 0;
}
