import Layout from "@/layout/layout";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { DeviceDetails } from "@/components/pages/device-details";

export default function Devices() {
  return (
    <Layout>
      <div className="p-8">
        <div className="mb-6">
          <h2 className="text-2xl font-semibold">Connected Devices</h2>
          <p className="text-gray-500">7 devices connected.</p>
        </div>
        <div>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="w-[100px]">Device Name</TableHead>
                <TableHead>IP</TableHead>
                <TableHead>OS</TableHead>
                <TableHead>Scale</TableHead>
                <TableHead>Orientation</TableHead>
                <TableHead>Refresh Rate</TableHead>
                <TableHead>Screen Size</TableHead>
                <TableHead></TableHead>
              </TableRow>
            </TableHeader>
            <TableBody className="border-t">
              {Array(7)
                .fill("")
                .map((_, index) => (
                  <TableRow key={index}>
                    <TableCell>Device {index + 1}</TableCell>
                    <TableCell>
                      192.168.1.{Math.floor(Math.random() * 255) + 1}
                    </TableCell>
                    <TableCell>
                      {
                        [
                          "Windows",
                          "MacOS",
                          "Linux",
                          "Android",
                          "iOS",
                          "iPadOS",
                        ][Math.floor(Math.random() * 6)]
                      }
                    </TableCell>
                    <TableCell>100%</TableCell>
                    <TableCell>
                      {Math.random() > 0.5 ? "Portrait" : "Landscape"}
                    </TableCell>
                    <TableCell>60 Hz</TableCell>
                    <TableCell>1080x1920</TableCell>
                    <TableCell className="text-center">
                      <DeviceDetails />
                    </TableCell>
                  </TableRow>
                ))}
            </TableBody>
          </Table>
        </div>
      </div>
    </Layout>
  );
}
