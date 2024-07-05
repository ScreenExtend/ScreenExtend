import * as React from "react"

import { cn } from "@/lib/utils"

export interface InputProps
  extends React.InputHTMLAttributes<HTMLInputElement> {hoverLabel: boolean}

const useFocus = <T extends HTMLElement>() => {
    const inputRef = React.useRef<T>(null);
    const setInputFocus = () => {inputRef.current && inputRef.current.focus()};
    return { inputRef, setInputFocus };
}

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
              "bg-transparent rounded-lg appearance-none peer flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background file:border-0 file:bg-transparent file:text-sm file:font-medium placeholder:text-muted-foreground focus-visible:outline-none disabled:cursor-not-allowed disabled:opacity-50 focus:border-blue-600 dark:focus:border-blue-500 border-2 focus:outline-none focus:ring-0",
              className
            )}
            ref={inputRef}
            disabled={disabled}
            {...props}
          />
          <label htmlFor={id} className="absolute text-sm text-gray-500 dark:text-gray-400 duration-300 transform -translate-y-4 scale-75 top-2 z-10 origin-[0] bg-white dark:bg-background px-2 peer-focus:px-2 peer-focus:text-blue-600 peer-focus:dark:text-blue-500 peer-placeholder-shown:scale-100 peer-placeholder-shown:-translate-y-1/2 peer-placeholder-shown:top-1/2 peer-focus:top-2 peer-focus:scale-75 peer-focus:-translate-y-4 rtl:peer-focus:translate-x-1/4 rtl:peer-focus:left-auto start-1" onClick={setInputFocus} style={{ cursor: disabled ? "not-allowed" : "text", opacity: disabled ? 0.5 : 1 }}>{placeholder}</label>
        </div>
      )
    } else {
      return (
        <>
          <input
            id={id}
            placeholder={placeholder}
            className={cn(
              "bg-transparent rounded-lg appearance-none peer flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background file:border-0 file:bg-transparent file:text-sm file:font-medium placeholder:text-muted-foreground focus-visible:outline-none disabled:cursor-not-allowed disabled:opacity-50 rounded-lg border bg-card text-card-foreground shadow-sm focus:border-blue-600 dark:focus:border-blue-500 border-2 focus:outline-none focus:ring-0",
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
