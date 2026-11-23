"use strict";

(() => {
  const button = document.getElementById("generate-secrets");
  const environment = document.getElementById("environment");
  const status = document.getElementById("secret-status");
  if (!button || !environment || !status) return;

  button.hidden = false;
  if (!globalThis.crypto || typeof globalThis.crypto.getRandomValues !== "function") {
    button.disabled = true;
    status.textContent = "Generación segura no disponible; usa un gestor de contraseñas.";
    return;
  }

  function randomHex32() {
    const bytes = new Uint8Array(16);
    globalThis.crypto.getRandomValues(bytes);
    return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  }

  button.addEventListener("click", () => {
    let count = 0;
    try {
      const next = environment.value.split("\n").map((line) => {
        const match = /^([A-Za-z_][A-Za-z0-9_]{0,63})=(GENERATE|REEMPLAZA_CON_UN_SECRETO_ALEATORIO)\r?$/.exec(line);
        if (!match || !/(?:^|_)(?:SECRET|PASSWORD|NONCE)$/.test(match[1])) return line;
        count += 1;
        return `${match[1]}=${randomHex32()}`;
      }).join("\n");
      if (next.length > environment.maxLength) {
        status.textContent = "No se han cambiado las variables: se superaría el tamaño máximo.";
        return;
      }
      environment.value = next;
      status.textContent = count
        ? `Secretos generados localmente: ${count}. Revisa y guarda los valores antes de enviar.`
        : "Añade una línea como APP_SECRET=GENERATE, PASSWORD=GENERATE o NONCE=GENERATE.";
    } catch {
      status.textContent = "No se han cambiado las variables: no se pudo generar un secreto seguro.";
    }
  });
})();
