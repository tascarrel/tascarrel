const path = require("node:path");

const server = process.env.TASCARREL_DESKTOP_SERVER;
const icon = path.resolve(__dirname, "icons/icon");

module.exports = {
  packagerConfig: {
    appBundleId: "dev.tascarrel.Tascarrel",
    appCategoryType: "public.app-category.developer-tools",
    asar: true,
    executableName: "tascarrel-desktop",
    extendInfo: {
      LSMinimumSystemVersion: "12.0",
    },
    extraResource: server ? [server] : [],
    icon,
    name: "Tascarrel",
    osxSign: {
      identity: "-",
    },
  },
  hooks: {
    prePackage: async () => {
      if (!server) {
        throw new Error("TASCARREL_DESKTOP_SERVER must name the Tascarrel server executable");
      }
    },
  },
  makers: [
    {
      name: "@electron-forge/maker-dmg",
      platforms: ["darwin"],
      config: {
        name: "Tascarrel",
      },
    },
    {
      name: "@electron-forge/maker-deb",
      platforms: ["linux"],
      config: {
        options: {
          categories: ["Development"],
          genericName: "Development Environment",
          icon: path.resolve(__dirname, "icons/icon.png"),
          maintainer: "Tascarrel Contributors",
          name: "tascarrel-desktop",
          productName: "Tascarrel",
        },
      },
    },
    {
      name: "@electron-forge/maker-rpm",
      platforms: ["linux"],
      config: {
        options: {
          categories: ["Development"],
          genericName: "Development Environment",
          icon: path.resolve(__dirname, "icons/icon.png"),
          name: "tascarrel-desktop",
          productName: "Tascarrel",
        },
      },
    },
  ],
};
