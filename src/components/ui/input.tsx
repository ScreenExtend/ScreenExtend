import * as React from "react"

import { cn, useFocus } from "@/lib/utils"

export interface InputProps
  extends React.InputHTMLAttributes<HTMLInputElement> {hoverLabel: boolean}

const Input = React.forwardRef<HTMLInputElement, InputProps>(
  ({ hoverLabel, id, className, placeholder, disabled, ...props }) => {
    const { inputRef, setInputFocus } = useFocus<HTMLInputElement>();
    if (hoverLabel) {
      return (
        <div className="relative">
          <input
            id={id}
            placeholder={" "}
            className={cn(
              "bg-transparent appearance-none peer flex h-10 w-full border-input bg-background px-3 py-2 text-sm ring-offset-background file:border-0 file:bg-transparent file:text-sm file:font-medium placeholder:text-muted-foreground focus-visible:outline-none disabled:cursor-not-allowed disabled:select-none disabled:opacity-50 focus:border-blue-600 dark:focus:border-blue-500 border-2 focus:outline-none focus:ring-0 rounded-md",
              className
            )}
            ref={inputRef}
            disabled={disabled}
            {...props}
          />
          <label htmlFor={id} className="absolute text-sm text-gray-500 dark:text-gray-400 duration-300 transform -translate-y-4 scale-75 top-2 z-10 origin-[0] bg-background px-2 peer-focus:px-2 peer-focus:text-blue-600 peer-focus:dark:text-blue-500 peer-placeholder-shown:scale-100 peer-placeholder-shown:-translate-y-1/2 peer-placeholder-shown:top-1/2 peer-focus:top-2 peer-focus:scale-75 peer-focus:-translate-y-4 rtl:peer-focus:translate-x-1/4 rtl:peer-focus:left-auto start-1 peer-disabled:text-opacity-50" onClick={setInputFocus} style={{ cursor: disabled ? "not-allowed" : "text" }}>{placeholder}</label>
        </div>
      )
    } else {
      return (
        <>
          <input
            id={id}
            placeholder={placeholder}
            className={cn(
              "bg-transparent appearance-none peer flex h-10 w-full border-input bg-background px-3 py-2 text-sm ring-offset-background file:border-0 file:bg-transparent file:text-sm file:font-medium placeholder:text-muted-foreground focus-visible:outline-none disabled:cursor-not-allowed disabled:select-none disabled:opacity-50 focus:border-blue-600 dark:focus:border-blue-500 border-2 focus:outline-none focus:ring-0 rounded-lg bg-card text-card-foreground shadow-sm",
              className
            )}
            ref={inputRef}
            disabled={disabled}
            {...props}
          />
        </>
      )
    }
  }
)
Input.displayName = "Input"

export { Input }
