/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      colors: {
        paper: "#f3f2ed",
        ink: "#242820",
        moss: "#526b54",
        clay: "#c56b4d",
        citron: "#d5d888",
      },
      fontFamily: {
        sans: ["Aptos", "Segoe UI", "sans-serif"],
        display: ["Iowan Old Style", "Palatino Linotype", "Georgia", "serif"],
      },
    },
  },
  plugins: [],
};