# radixdlt-connect-auto

Habla con la **Radix Wallet** por **iroh** o por **WebRTC**, el que pueda, sin tener que elegir.

Radix Connect llega a la wallet del móvil por dos transportes: WebRTC, que hablan todas las Radix
Wallet, e iroh (QUIC), para las que lo tienen activado. Este crate usa los dos, para que ni tú ni
quien use tu aplicación tengáis que elegir, y cada petición vaya por el camino más rápido:

- **Un solo QR** empareja por los dos a la vez: la wallet se enlaza por iroh si lo tiene activado, y
  si no por WebRTC. El enlace guardado apunta cuál.
- **Cada petición** de un enlace emparejado por iroh sale por iroh al momento si su conexión está
  arriba. Si no, **compite por los dos**: el primer transporte listo para llevarla la lleva — y solo
  ese, así que la wallet nunca recibe una petición dos veces — y la respuesta se espera por los dos.
  Una petición nunca es más lenta que por WebRTC, y es tan rápida como iroh cuando iroh está.
- **Siempre listo:** un proceso que sigue en marcha (un demonio, un agente) puede `keep_listening`
  en sus enlaces: la wallet se conecta en cuanto se abre su app y la siguiente petición sale al momento.
- **Eliminar un enlace** se lo dice a la wallet por cada transporte en el que pueda estar, para que
  también elimine su lado.

Mira el ejemplo de [`README.md`](README.md). Un `Error` dice hasta dónde llegó la petición, sea cual
sea el transporte que la llevó.

**¿Solo un transporte?** Usa directamente [`radixdlt-connect-webrtc`](../connect-webrtc) o
[`radixdlt-connect-iroh`](../connect-iroh).

## Licencia

MIT OR Apache-2.0.
